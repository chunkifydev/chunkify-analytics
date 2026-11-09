//! Resolve file-backed settings without mutating the process environment.
mod file;
use std::{collections::HashMap, env, ffi::OsString, path::Path};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const ENVIRONMENT_OVERRIDES: &[(&str, &str)] = &[
    ("CHUNKIFY_ADMIN_KEY", "admin_key"),
    ("CHUNKIFY_BIND", "server.bind"),
    ("CHUNKIFY_GEOIP_DATABASE", "server.geoip_database"),
    ("CHUNKIFY_CLICKHOUSE_URL", "clickhouse.url"),
    ("CHUNKIFY_CLICKHOUSE_DATABASE", "clickhouse.database"),
    ("CHUNKIFY_CLICKHOUSE_USER", "clickhouse.user"),
    ("CHUNKIFY_CLICKHOUSE_PASSWORD", "clickhouse.password"),
    ("CHUNKIFY_CLICKHOUSE_NODE_ID", "clickhouse.node_id"),
    ("CHUNKIFY_MCP_URL", "mcp.url"),
    ("CHUNKIFY_MCP_SECRET_KEY", "mcp.secret_key"),
];

fn environment_field(name: &str) -> Option<&'static str> {
    ENVIRONMENT_OVERRIDES
        .iter()
        .find_map(|(variable, field)| (*variable == name).then_some(*field))
}

#[derive(Default)]
pub struct Settings {
    values: HashMap<String, OsString>,
    pub rate_limits: crate::limits::Config,
}

impl Settings {
    pub fn load(env_file: Option<&Path>, config_file: Option<&Path>) -> Result<Self> {
        let mut settings = file::load(config_file)?;
        let file = env_file.unwrap_or_else(|| Path::new(".env"));
        match dotenvy::from_path_iter(file) {
            Ok(entries) => {
                let mut declared = std::collections::HashSet::new();
                for entry in entries {
                    let (name, value) = entry
                        .map_err(|_| format!("Invalid environment file: {}", file.display()))?;
                    if declared.insert(name.clone())
                        && let Some(field) = environment_field(&name)
                    {
                        settings.values.insert(field.into(), value.into());
                    }
                }
            }
            Err(dotenvy::Error::Io(error))
                if env_file.is_none() && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(format!("Cannot read environment file: {}", file.display()).into());
            }
        }
        for (name, value) in env::vars_os() {
            if let Ok(name) = name.into_string()
                && let Some(field) = environment_field(&name)
            {
                settings.values.insert(field.into(), value);
            }
        }
        Ok(settings)
    }

    pub fn var(&self, name: &str) -> std::result::Result<String, env::VarError> {
        self.values
            .get(name)
            .ok_or(env::VarError::NotPresent)?
            .clone()
            .into_string()
            .map_err(env::VarError::NotUnicode)
    }

    pub fn var_os(&self, name: &str) -> Option<OsString> {
        self.values.get(name).cloned()
    }
}
