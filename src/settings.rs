//! Resolve file-backed settings without mutating the process environment.
use std::{collections::HashMap, env, ffi::OsString, path::Path};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Default)]
pub struct Settings {
    values: HashMap<String, OsString>,
}

impl Settings {
    pub fn load(env_file: Option<&Path>) -> Result<Self> {
        let mut settings = Self::default();
        let file = env_file.unwrap_or_else(|| Path::new(".env"));
        match dotenvy::from_path_iter(file) {
            Ok(entries) => {
                for entry in entries {
                    let (name, value) = entry
                        .map_err(|_| format!("Invalid environment file: {}", file.display()))?;
                    settings.values.entry(name).or_insert_with(|| value.into());
                }
            }
            Err(dotenvy::Error::Io(error))
                if env_file.is_none() && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(format!("Cannot read environment file: {}", file.display()).into());
            }
        }
        for (name, value) in env::vars_os() {
            if let Ok(name) = name.into_string() {
                settings.values.insert(name, value);
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
