//! Project budgets are shared by HTTP, MCP, credentials and export links.
use crate::api::ApiError;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter, clock::Clock};
use serde::Deserialize;
use std::{num::NonZeroU32, sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub read_per_minute: u32,
    pub read_burst: u32,
    pub read_concurrent_per_project: u32,
    pub read_concurrent: u32,
    pub export_links_per_minute: u32,
    pub export_downloads_per_minute: u32,
    pub export_concurrent_per_project: u32,
    pub export_concurrent: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            read_per_minute: 120,
            read_burst: 20,
            read_concurrent_per_project: 2,
            read_concurrent: 8,
            export_links_per_minute: 10,
            export_downloads_per_minute: 2,
            export_concurrent_per_project: 1,
            export_concurrent: 4,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value, maximum) in [
            ("read_per_minute", self.read_per_minute, 1_000_000),
            ("read_burst", self.read_burst, 1_000_000),
            (
                "read_concurrent_per_project",
                self.read_concurrent_per_project,
                256,
            ),
            ("read_concurrent", self.read_concurrent, 256),
            (
                "export_links_per_minute",
                self.export_links_per_minute,
                1_000_000,
            ),
            (
                "export_downloads_per_minute",
                self.export_downloads_per_minute,
                1_000_000,
            ),
            (
                "export_concurrent_per_project",
                self.export_concurrent_per_project,
                64,
            ),
            ("export_concurrent", self.export_concurrent, 64),
        ] {
            if !(1..=maximum).contains(&value) {
                return Err(format!(
                    "rate_limits.{name} must be between 1 and {maximum}"
                ));
            }
        }
        if self.read_concurrent_per_project > self.read_concurrent
            || self.export_concurrent_per_project > self.export_concurrent
        {
            return Err(
                "rate_limits per-project concurrency cannot exceed server concurrency".into(),
            );
        }
        Ok(())
    }
}

pub struct Rate(DefaultDirectRateLimiter);
impl Rate {
    fn new(per_minute: u32, burst: u32) -> Self {
        let quota = Quota::per_minute(NonZeroU32::new(per_minute).expect("validated rate"))
            .allow_burst(NonZeroU32::new(burst).expect("validated burst"));
        Self(RateLimiter::direct(quota))
    }

    pub fn take(&self, message: &str) -> Result<(), ApiError> {
        self.0
            .check()
            .map_err(|until| ApiError::limited(message, until.wait_time_from(self.0.clock().now())))
    }
}

pub struct Budget {
    rate: Rate,
    concurrent: Arc<Semaphore>,
}
pub struct Admission {
    _project: OwnedSemaphorePermit,
    _server: OwnedSemaphorePermit,
}
impl Budget {
    fn new(per_minute: u32, burst: u32, concurrent: u32) -> Self {
        Self {
            rate: Rate::new(per_minute, burst),
            concurrent: Arc::new(Semaphore::new(concurrent as usize)),
        }
    }

    pub fn admit(&self, server: &Arc<Semaphore>, operation: &str) -> Result<Admission, ApiError> {
        let project = self.concurrent.clone().try_acquire_owned().map_err(|_| {
            ApiError::limited(
                &format!("Project {operation} concurrency limit exceeded"),
                Duration::from_secs(1),
            )
        })?;
        let server = server.clone().try_acquire_owned().map_err(|_| {
            ApiError::limited(
                &format!("Server {operation} concurrency limit exceeded"),
                Duration::from_secs(1),
            )
        })?;
        self.rate
            .take(&format!("Project {operation} rate limit exceeded"))?;
        Ok(Admission {
            _project: project,
            _server: server,
        })
    }
}

pub struct Project {
    pub reads: Budget,
    pub links: Rate,
    pub downloads: Budget,
}
impl Project {
    pub fn new(config: &Config) -> Self {
        Self {
            reads: Budget::new(
                config.read_per_minute,
                config.read_burst,
                config.read_concurrent_per_project,
            ),
            links: Rate::new(
                config.export_links_per_minute,
                config.export_links_per_minute,
            ),
            downloads: Budget::new(
                config.export_downloads_per_minute,
                config.export_downloads_per_minute,
                config.export_concurrent_per_project,
            ),
        }
    }
}

#[derive(Clone)]
pub struct Reads {
    projects: Arc<crate::projects::Registry>,
    concurrent: Arc<Semaphore>,
}
impl Reads {
    pub fn new(config: &Config, projects: Arc<crate::projects::Registry>) -> Self {
        Self {
            projects,
            concurrent: Arc::new(Semaphore::new(config.read_concurrent as usize)),
        }
    }

    pub fn admit(&self, project: &str) -> Result<Admission, ApiError> {
        self.projects
            .access(project)?
            .limits
            .reads
            .admit(&self.concurrent, "read")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_and_rate_budgets_release_slots_after_rejection() {
        let config = Config {
            read_per_minute: 1,
            read_burst: 1,
            read_concurrent_per_project: 1,
            read_concurrent: 2,
            ..Config::default()
        };
        let a = Project::new(&config);
        let b = Project::new(&config);
        let c = Project::new(&config);
        let server = Arc::new(Semaphore::new(2));
        let first = a.reads.admit(&server, "read").unwrap();
        assert!(a.reads.admit(&server, "read").is_err());
        let second = b.reads.admit(&server, "read").unwrap();
        assert!(c.reads.admit(&server, "read").is_err());
        drop(first);
        let error = a.reads.admit(&server, "read").err().unwrap();
        assert!(
            error
                .retry_after
                .is_some_and(|seconds| (1..=60).contains(&seconds))
        );
        assert_eq!(server.available_permits(), 1);
        let third = c.reads.admit(&server, "read").unwrap();
        drop((second, third));
        assert_eq!(server.available_permits(), 2);
    }
}
