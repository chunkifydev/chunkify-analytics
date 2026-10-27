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
    sync::{Arc, Mutex, RwLock},
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
    Public,
    Secret,
}

impl Permission {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "public" => Ok(Self::Public),
            "secret" => Ok(Self::Secret),
            _ => Err("Key kind must be public or secret".into()),
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

pub fn generate_key(permission: Permission) -> String {
    let prefix = if permission == Permission::Public {
        "ck_public"
    } else {
        "ck_secret"
    };
    format!(
        "{prefix}_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub fn credential(name: &str, permission: Permission) -> (Credential, String) {
    let secret = generate_key(permission);
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
    pub retention_seconds: Option<u64>,
    pub ingest_per_second: Option<u32>,
    pub queue_bytes: Option<usize>,
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
                    id: "primary-public".into(),
                    name: "Primary public".into(),
                    permission: Permission::Public,
                    hash: hash(&keys.public_key),
                },
                Credential {
                    id: "primary-secret".into(),
                    name: "Primary secret".into(),
                    permission: Permission::Secret,
                    hash: hash(&keys.secret_key),
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
        if project
            .settings
            .retention_seconds
            .is_some_and(|seconds| seconds > crate::retention::MAX_SECONDS)
        {
            return Err("Project retention_seconds is too large".into());
        }
        if project.settings.ingest_per_second == Some(0) {
            return Err("Project ingest rate must be positive".into());
        }
        if let Some(value) = project.settings.queue_bytes
            && !(4096..=268435456).contains(&value)
        {
            return Err("Project queue limits must be between 4096 and 268435456 bytes".into());
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
    origins: RwLock<Vec<String>>,
    pub rate: Mutex<RateLimit>,
    budget: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    pub queue_bytes: usize,
}

// Held until the prepared write is complete.
pub struct Admission {
    _bytes: OwnedSemaphorePermit,
    _request: OwnedSemaphorePermit,
}
impl Access {
    pub fn allows_origin(&self, origin: &str) -> bool {
        self.origins
            .read()
            .expect("origins lock")
            .iter()
            .any(|value| value == origin)
    }

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
    pub retention: RwLock<crate::retention::Policy>,
    pub projects: HashMap<String, Arc<Access>>,
    credentials: RwLock<HashMap<String, (Permission, Resolved)>>,
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
            retention: RwLock::new(crate::retention::Policy::new(
                config.retention_seconds,
                &projects,
            )),
            projects: HashMap::new(),
            credentials: RwLock::new(HashMap::new()),
        };
        for project in projects {
            let queue_bytes = project
                .settings
                .queue_bytes
                .unwrap_or(config.batch.queue_bytes / count)
                .min(config.batch.queue_bytes);
            let access = Arc::new(Access {
                id: project.id.clone(),
                origins: RwLock::new(effective_origins(
                    &config.allowed_origins,
                    project.settings.allowed_origins.as_deref(),
                )),
                rate: Mutex::new(RateLimit::new(
                    project
                        .settings
                        .ingest_per_second
                        .unwrap_or((config.ingest_per_second / count as u32).max(1)),
                )),
                budget: Arc::new(Semaphore::new(queue_bytes)),
                queue_bytes,
                requests: Arc::new(Semaphore::new((config.batch.queue_requests / count).max(1))),
            });
            for key in project.keys {
                registry
                    .credentials
                    .get_mut()
                    .expect("credentials lock")
                    .insert(
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
    // Only credentials change while serving. Reuse the exact Access objects so
    // rate tokens, admission permits, origins and project budgets stay intact.
    pub fn replace_keys(&self, keys: &Keys) {
        let mut credentials = HashMap::new();
        for project in projects(keys) {
            let access = &self.projects[&project.id];
            for key in project.keys {
                credentials.insert(
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
        }
        *self.credentials.write().expect("credentials lock") = credentials;
    }

    pub fn replace_origins(&self, project: &str, origins: Vec<String>) {
        *self.projects[project]
            .origins
            .write()
            .expect("origins lock") = origins;
    }

    pub fn authorize(&self, secret: &str, permission: Permission) -> Result<Resolved, ApiError> {
        self.credentials
            .read()
            .expect("credentials lock")
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

pub fn effective_origins(shared: &[String], extra: Option<&[String]>) -> Vec<String> {
    let mut origins = shared.to_vec();
    for origin in extra.unwrap_or_default() {
        if !origins.contains(origin) {
            origins.push(origin.clone());
        }
    }
    origins
}

pub fn apply_command(
    keys: &mut Keys,
    command: &str,
    args: &[String],
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut list = projects(keys);
    let mut output = serde_json::json!({"restart_required": true});
    match (command, args) {
            ("project-list", []) => {
                return Ok(serde_json::json!(list.iter().map(|p| serde_json::json!({"id": p.id, "name": p.name, "settings": p.settings,
                    "keys": p.keys.iter().map(|k| serde_json::json!({"id": k.id, "name": k.name, "permission": k.permission})).collect::<Vec<_>>() })).collect::<Vec<_>>()));
            }
            ("rotate-key", [kind]) => {
                let permission = Permission::parse(kind)?;
                let (mut key, secret) = credential(&format!("Primary {kind}"), permission);
                key.id = format!("primary-{kind}");
                if permission == Permission::Public { keys.public_key = secret; }
                else { keys.secret_key = secret; }
                // Keep the legacy format until named projects/keys are used.
                if keys.projects.is_none() { return Ok(output); }
                let project = list.iter_mut().find(|p| p.id == DEFAULT_PROJECT).ok_or("Missing default project")?;
                project.keys.retain(|k| k.id != key.id);
                project.keys.push(key);
            }
            ("project-create", [name]) => {
                let id = uuid::Uuid::new_v4().to_string();
                let (ingest, public_key) = credential("Primary public", Permission::Public);
                let (project_key, secret_key) = credential("Primary secret", Permission::Secret);
                output = serde_json::json!({"project_id": id, "name": name, "public_key": public_key, "secret_key": secret_key, "restart_required": true});
                list.push(Project { id, name: name.clone(), settings: Settings::default(), keys: vec![ingest, project_key] });
            }
            ("project-rename", [id, name]) => {
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                project.name = name.clone();
            }
            ("project-retention", [id, value]) => {
                let seconds = if value == "inherit" { None } else {
                    Some(value.parse::<u64>().map_err(|_| "Expected retention seconds as a non-negative whole number, or inherit")?)
                };
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                project.settings.retention_seconds = seconds;
                output = serde_json::json!({"project_id": id, "retention_seconds": seconds, "restart_required": true});
            }
            ("project-origins", [id, value]) => {
                let origins: Vec<String> = serde_json::from_str(value)?;
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                project.settings.allowed_origins = Some(origins.clone());
                output = serde_json::json!({"project_id": id, "allowed_origins": origins, "restart_required": true});
            }
            ("project-configure", [id, file]) => {
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                project.settings = serde_json::from_slice(&std::fs::read(file)?)?;
            }
            ("key-create", [id, kind, name]) => {
                let project = list.iter_mut().find(|p| &p.id == id).ok_or("Unknown project")?;
                let (key, secret) = credential(name, Permission::parse(kind)?);
                output = serde_json::json!({"project_id": id, "key_id": key.id, "key": secret, "permission": key.permission, "restart_required": true});
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
                if id == DEFAULT_PROJECT && key_id == "primary-public" { keys.public_key = secret.clone(); }
                if id == DEFAULT_PROJECT && key_id == "primary-secret" { keys.secret_key = secret.clone(); }
                output = serde_json::json!({"project_id": id, "key_id": key_id, "key": secret, "permission": old.permission, "restart_required": true});
            }
            _ => return Err("Expected project list | project create <id> [name] | project rename <id> <name> | project configure <id> <settings.json> | project retention <id> <seconds|inherit> | key create <project> <public|secret> <name> | key rotate <project> <key-id> | key revoke <project> <key-id>".into()),
        }
    validate(&list)?;
    keys.projects = Some(list);
    Ok(output)
}
