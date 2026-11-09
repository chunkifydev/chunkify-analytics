//! Receipt-time expiration policy, loaded with the administrator's project settings.
use crate::projects::Project;

pub const MAX_SECONDS: u64 = (i64::MAX / 1000) as u64;

#[derive(Clone)]
pub struct Policy {
    default_seconds: u64,
    // Projects with their own retention or an expiration floor.
    overrides: Vec<Override>,
}

#[derive(Clone)]
struct Override {
    id: String,
    seconds: Option<u64>,
    expired_before_ms: i64,
}

impl Policy {
    // Configuration and project validation bound durations before construction.
    pub fn new(default_seconds: u64, projects: &[Project]) -> Self {
        Self {
            default_seconds,
            overrides: projects
                .iter()
                .filter(|project| {
                    project.settings.retention_seconds.is_some()
                        || project.expired_before_ms.is_some()
                })
                .map(|project| Override {
                    id: project.id.clone(),
                    seconds: project.settings.retention_seconds,
                    expired_before_ms: project.expired_before_ms.unwrap_or(i64::MIN),
                })
                .collect(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.default_seconds > 0
            || self.overrides.iter().any(|project| {
                project.seconds.is_some_and(|seconds| seconds > 0)
                    || project.expired_before_ms > i64::MIN
            })
    }

    /// Events received before this time are expired for the project.
    pub fn project_cutoff(&self, project: &str, now_ms: i64) -> i64 {
        let floor = self
            .overrides
            .iter()
            .find(|entry| entry.id == project)
            .map_or(i64::MIN, |entry| entry.expired_before_ms);
        cutoff(now_ms, self.project_seconds(project)).max(floor)
    }

    /// Projects whose cutoff differs from the installation default.
    pub fn overrides(&self) -> impl Iterator<Item = &str> {
        self.overrides.iter().map(|project| project.id.as_str())
    }

    pub fn default_cutoff(&self, now_ms: i64) -> i64 {
        cutoff(now_ms, self.default_seconds)
    }

    pub fn project_seconds(&self, project: &str) -> u64 {
        self.overrides
            .iter()
            .find(|entry| entry.id == project)
            .and_then(|entry| entry.seconds)
            .unwrap_or(self.default_seconds)
    }
}

fn cutoff(now_ms: i64, seconds: u64) -> i64 {
    if seconds == 0 {
        // No stored timestamp can precede this value, so expiration is disabled.
        i64::MIN
    } else {
        now_ms.saturating_sub((seconds * 1000) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::Settings;

    fn project(id: &str, seconds: Option<u64>) -> Project {
        Project {
            deleted: false,
            expired_before_ms: None,
            id: id.into(),
            name: id.into(),
            keys: Vec::new(),
            settings: Settings {
                retention_seconds: seconds,
                ..Settings::default()
            },
        }
    }

    #[test]
    fn cutoffs_respect_project_overrides_and_exact_receipt_boundary() {
        let policy = Policy::new(
            1,
            &[
                project("inherit", None),
                project("long", Some(60)),
                project("short", Some(1)),
                project("keep", Some(0)),
            ],
        );
        assert!(policy.enabled());
        assert_eq!(policy.project_cutoff("inherit", 10500), 9500);
        assert_eq!(policy.project_cutoff("unlisted", 10500), 9500);
        assert_eq!(policy.project_cutoff("long", 10500), 10500 - 60_000);
        assert_eq!(policy.project_cutoff("short", 10500), 9500);
        assert_eq!(policy.project_cutoff("keep", 10500), i64::MIN);
        assert_eq!(policy.default_cutoff(10500), 9500);
        let mut overrides: Vec<_> = policy.overrides().collect();
        overrides.sort();
        assert_eq!(overrides, ["keep", "long", "short"]);
    }

    #[test]
    fn an_expiration_floor_keeps_old_events_hidden_when_retention_grows() {
        let mut keep = project("keep", Some(0));
        keep.expired_before_ms = Some(5000);
        let mut inherit = project("inherit", None);
        inherit.expired_before_ms = Some(9800);
        let policy = Policy::new(0, &[keep, inherit]);
        assert!(policy.enabled());
        assert_eq!(policy.project_cutoff("keep", 10000), 5000);
        assert_eq!(policy.project_cutoff("inherit", 10000), 9800);
        assert_eq!(policy.project_cutoff("other", 10000), i64::MIN);
        // A shorter policy than the floor still applies.
        let mut short = project("short", Some(1));
        short.expired_before_ms = Some(100);
        assert_eq!(
            Policy::new(0, &[short]).project_cutoff("short", 10000),
            9000
        );
    }

    #[test]
    fn disabled_default_still_allows_project_expiration_and_large_durations() {
        let disabled = Policy::new(0, &[]);
        assert!(!disabled.enabled());
        assert_eq!(disabled.project_cutoff("default", 10000), i64::MIN);
        let policy = Policy::new(
            0,
            &[
                project("short", Some(1)),
                project("long", Some(MAX_SECONDS)),
            ],
        );
        assert!(policy.enabled());
        assert_eq!(policy.project_cutoff("short", 10000), 9000);
        assert_eq!(
            policy.project_cutoff("long", 10000),
            10000 - (MAX_SECONDS * 1000) as i64
        );
        assert_eq!(policy.project_cutoff("default", 10000), i64::MIN);
    }
}
