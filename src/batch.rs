//! Write batching limits. Each server collects requests into batches; requests
//! that arrive while one batch is saved form the next, up to these limits.
use std::time::Duration;

#[derive(Clone)]
pub struct BatchConfig {
    pub max_events: usize,
    pub max_bytes: usize,
    pub delay: Duration,
    pub queue_bytes: usize,
    pub queue_requests: usize,
}

impl BatchConfig {
    pub fn load(settings: &crate::settings::Settings) -> Result<Self, Box<dyn std::error::Error>> {
        fn number(
            settings: &crate::settings::Settings,
            name: &str,
            default: usize,
            min: usize,
            max: usize,
        ) -> Result<usize, Box<dyn std::error::Error>> {
            let value = settings
                .var(name)
                .unwrap_or_else(|_| default.to_string())
                .parse::<usize>()
                .map_err(|_| format!("{name} must be between {min} and {max}"))?;
            if !(min..=max).contains(&value) {
                return Err(format!("{name} must be between {min} and {max}").into());
            }
            Ok(value)
        }
        let max_bytes = number(settings, "batch.max_bytes", 10485760, 1, 67108864)?;
        Ok(Self {
            max_events: number(settings, "batch.max_events", 10000, 1, 10000)?,
            max_bytes,
            delay: Duration::from_millis(number(settings, "batch.delay_ms", 10, 0, 1000)? as u64),
            queue_bytes: number(
                settings,
                "batch.queue_max_bytes",
                33554432,
                max_bytes,
                268435456,
            )?,
            queue_requests: number(settings, "batch.queue_max_requests", 4096, 1, 4096)?,
        })
    }
}
