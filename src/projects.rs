//! Project ownership and credentials. HTTP callers receive a resolved project;
//! event properties and query parameters never choose the storage scope.
use crate::{
    api::ApiError,
    config::{Config, Keys},
};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const DEFAULT_PROJECT: &str = "default";
pub fn default_project() -> String {
    DEFAULT_PROJECT.into()
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Ingest,
    Read,
}

impl Permission {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "ingest" => Ok(Self::Ingest),
            "read" => Ok(Self::Read),
            _ => Err("Key kind must be ingest or read".into()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub id: String,
    pub name: String,
    pub permission: Permission,
    pub hash: String,
}

pub fn hash(secret: &str) -> String {
    digest(&SHA256, secret.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn credential(name: &str, permission: Permission) -> (Credential, String) {
    let prefix = if permission == Permission::Ingest {
        "cki"
    } else {
        "ckr"
    };
    let secret = format!(
        "{prefix}_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    (
        Credential {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            permission,
            hash: hash(&secret),
        },
        secret,
    )
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub allowed_origins: Option<Vec<String>>,
    pub ingest_per_second: Option<u32>,
    pub queue_bytes: Option<usize>,
    pub spool_bytes: Option<usize>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub keys: Vec<Credential>,
    #[serde(default)]
    pub settings: Settings,
}

pub fn projects(keys: &Keys) -> Vec<Project> {
    keys.projects.clone().unwrap_or_else(|| {
        vec![Project {
            id: default_project(),
            name: "Default project".into(),
            settings: Settings::default(),
            keys: vec![
                Credential {
                    id: "primary-ingest".into(),
                    name: "Primary ingest".into(),
                    permission: Permission::Ingest,
                    hash: hash(&keys.ingest_key),
                },
                Credential {
                    id: "primary-read".into(),
                    name: "Primary read".into(),
                    permission: Permission::Read,
                    hash: hash(&keys.read_token),
                },
            ],
        }]
    })
}

pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(
            "Project IDs must have 1..64 ASCII letters, digits, hyphens or underscores".into(),
        );
    }
    Ok(())
}

pub fn validate(projects: &[Project]) -> Result<(), String> {
    if projects.is_empty()
        || projects.len() > 1024
        || !projects.iter().any(|p| p.id == DEFAULT_PROJECT)
    {
        return Err("Expected 1..1024 projects including default".into());
    }
    let mut ids = HashSet::new();
    let mut hashes = HashSet::new();
    for project in projects {
        validate_id(&project.id)?;
        if project.name.trim().is_empty() || project.name.chars().count() > 128 {
            return Err("Project names must have 1..128 characters".into());
        }
        if !ids.insert(&project.id) {
            return Err("Duplicate project ID".into());
        }
        let mut key_ids = HashSet::new();
        if project.keys.len() > 64 {
            return Err("At most 64 keys per project".into());
        }
        for key in &project.keys {
            if key.id.is_empty()
                || key.id.len() > 64
                || key.name.is_empty()
                || key.name.len() > 128
                || !key_ids.insert(&key.id)
                || key.hash.len() != 64
                || !key
                    .hash
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                || !hashes.insert(&key.hash)
            {
                return Err("Invalid or duplicate project credential".into());
            }
        }
        if project.settings.ingest_per_second == Some(0) {
            return Err("Project ingest rate must be positive".into());
        }
        for value in [project.settings.queue_bytes, project.settings.spool_bytes]
            .into_iter()
            .flatten()
        {
            if !(4096..=268435456).contains(&value) {
                return Err("Project queue limits must be between 4096 and 268435456 bytes".into());
            }
        }
        if let Some(origins) = &project.settings.allowed_origins {
            for origin in origins {
                crate::config::validate_origin(origin)?;
            }
        }
    }
    Ok(())
}

pub struct RateLimit {
    tokens: f64,
    capacity: f64,
    updated: Instant,
}
impl RateLimit {
    pub fn new(rate: u32) -> Self {
        Self {
            tokens: rate.into(),
            capacity: rate.into(),
            updated: Instant::now(),
        }
    }
    pub fn take(&mut self) -> Result<(), ApiError> {
        let now = Instant::now();
        self.tokens = (self.tokens
            + now.duration_since(self.updated).as_secs_f64() * self.capacity)
            .min(self.capacity);
        self.updated = now;
        if self.tokens < 1.0 {
            return Err(ApiError(
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "Ingestion rate limit exceeded".into(),
            ));
        }
        self.tokens -= 1.0;
        Ok(())
    }
}

pub struct Access {
    pub id: String,
    pub origins: Vec<String>,
    pub rate: Mutex<RateLimit>,
    budget: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    pub queue_bytes: usize,
    pub spool_bytes: usize,
}

// Held until the prepared write or durable acknowledgement is complete.
pub struct Admission {
    _bytes: OwnedSemaphorePermit,
    _request: OwnedSemaphorePermit,
}
impl Access {
    pub fn admit(&self, bytes: usize) -> Result<Admission, ApiError> {
        if bytes > self.queue_bytes {
            return Err(ApiError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds project queue limit".into(),
            ));
        }
        Ok(Admission {
            _bytes: self
                .budget
                .clone()
                .try_acquire_many_owned(bytes as u32)
                .map_err(|_| ApiError::unavailable())?,
            _request: self
                .requests
                .clone()
                .try_acquire_owned()
                .map_err(|_| ApiError::unavailable())?,
        })
    }
}

pub struct Registry {
    pub projects: HashMap<String, Arc<Access>>,
    credentials: HashMap<String, (Permission, Resolved)>,
}

#[derive(Clone)]
pub struct Resolved {
    pub project: Arc<Access>,
    pub key_id: String,
}

impl Registry {
    pub fn new(keys: &Keys, config: &Config) -> Result<Self, String> {
        let projects = projects(keys);
        validate(&projects)?;
        let count = projects.len();
        let mut registry = Self {
            projects: HashMap::new(),
            credentials: HashMap::new(),
        };
        let global_spool = config.spool.as_ref().map_or(67108864, |s| s.max_bytes);
        for project in projects {
            let queue_bytes = project
                .settings
                .queue_bytes
                .unwrap_or(config.batch.queue_bytes / count)
                .min(config.batch.queue_bytes);
            let access = Arc::new(Access {
                id: project.id.clone(),
                origins: project
                    .settings
                    .allowed_origins
                    .unwrap_or_else(|| config.allowed_origins.clone()),
                rate: Mutex::new(RateLimit::new(
                    project
                        .settings
                        .ingest_per_second
                        .unwrap_or((config.ingest_per_second / count as u32).max(1)),
                )),
                budget: Arc::new(Semaphore::new(queue_bytes)),
                queue_bytes,
                requests: Arc::new(Semaphore::new((config.batch.queue_requests / count).max(1))),
                spool_bytes: project
                    .settings
                    .spool_bytes
                    .unwrap_or(global_spool / count)
                    .min(global_spool),
            });
            for key in project.keys {
                registry.credentials.insert(
                    key.hash,
                    (
                        key.permission,
                        Resolved {
                            project: access.clone(),
                            key_id: key.id,
                        },
                    ),
                );
            }
            registry.projects.insert(project.id, access);
        }
        Ok(registry)
    }
    pub fn authorize(&self, secret: &str, permission: Permission) -> Result<Resolved, ApiError> {
        self.credentials
            .get(&hash(secret))
            .filter(|(p, _)| *p == permission)
            .map(|(_, project)| project.clone())
            .ok_or_else(|| {
                ApiError(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "Missing or invalid credentials".into(),
                )
            })
    }
}

impl Config {
    pub fn project_command(
        &self,
        command: &str,
        args: &[String],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _lock = self.lock()?;
        let mut keys = self.keys()?;
        let mut list = projects(&keys);
        let mut output = serde_json::json!({"restart_required": true});
        match (command, args) {
            ("project-list", []) => {
                println!("{}", serde_json::to_string(&list.iter().map(|p| serde_json::json!({"id": p.id, "name": p.name, "settings": p.settings,
                    "keys": p.keys.iter().map(|k| serde_json::json!({"id": k.id, "name": k.name, "permission": k.permission})).collect::<Vec<_>>() })).collect::<Vec<_>>())?);
                return Ok(());
            }
            ("project-create", [id]) | ("project-create", [id, _]) => {
                validate_id(id)?;
                if list.iter().any(|p| &p.id == id) { return Err("Project already exists".into()); }
                let (ingest, ingest_key) = credential("Primary ingest", Permission::Ingest);
                let (read, read_token) = credential("Primary read", Permission::Read);
                let name = args.get(1).unwrap_or(id).clone();
                output = serde_json::json!({"project_id": id, "name": name, "ingest_key": ingest_key, "read_token": read_token, "restart_required": true});
                list.push(Project { id: id.clone(), name, settings: Settings::default(), keys: vec![ingest, read] });
            }
            ("project-rename", [id, name]) => {
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                project.name = name.clone();
            }
            ("project-configure", [id, file]) => {
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                project.settings = serde_json::from_slice(&std::fs::read(file)?)?;
            }
            ("key-create", [id, kind, name]) => {
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                let (key, secret) = credential(name, Permission::parse(kind)?);
                output = serde_json::json!({"project_id": id, "key_id": key.id, "secret": secret, "restart_required": true});
                project.keys.push(key);
            }
            ("key-revoke", [id, key_id]) => {
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                let index = project.keys.iter().position(|k| &k.id == key_id).ok_or("Unknown key")?;
                project.keys.remove(index);
            }
            ("key-rotate", [id, key_id]) => {
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                let old = project.keys.iter_mut().find(|k| &k.id == key_id).ok_or("Unknown key")?;
                let (new, secret) = credential(&old.name, old.permission);
                old.hash = new.hash;
                if id == DEFAULT_PROJECT && key_id == "primary-ingest" { keys.ingest_key = secret.clone(); }
                if id == DEFAULT_PROJECT && key_id == "primary-read" { keys.read_token = secret.clone(); }
                output = serde_json::json!({"project_id": id, "key_id": key_id, "secret": secret, "restart_required": true});
            }
            _ => return Err("Expected project-list | project-create <id> [name] | project-rename <id> <name> | project-configure <id> <settings.json> | key-create <project> <ingest|read> <name> | key-rotate <project> <key-id> | key-revoke <project> <key-id>".into()),
        }
        validate(&list)?;
        keys.projects = Some(list);
        self.save_keys(&keys)?;
        println!("{output}");
        Ok(())
    }
}
