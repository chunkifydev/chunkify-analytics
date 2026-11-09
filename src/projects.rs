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

pub const MAX_PROJECTS: usize = 1024;
pub const MAX_KEYS: usize = 64;
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
    #[serde(default)]
    pub read_only: bool,
    pub hash: String,
}

impl Credential {
    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id, "name": self.name, "permission": self.permission,
            "read_only": self.read_only,
        })
    }
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
            read_only: false,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub allowed_origins: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub retention_seconds: Option<Option<u64>>,
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl Update {
    pub fn validate(&mut self) -> Result<(), String> {
        if self.name.is_none() && self.allowed_origins.is_none() && self.retention_seconds.is_none()
        {
            return Err("Provide name, allowed_origins or retention_seconds".into());
        }
        if let Some(name) = &self.name {
            validate_name(name)?;
        }
        if let Some(origins) = &mut self.allowed_origins {
            normalize_origins(origins)?;
        }
        validate_retention(self.retention_seconds.flatten())?;
        Ok(())
    }
}

pub fn validate_retention(seconds: Option<u64>) -> Result<(), String> {
    if seconds.is_some_and(|seconds| seconds > crate::retention::MAX_SECONDS) {
        return Err("Project retention_seconds is too large".into());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewKey {
    pub name: String,
    pub permission: Permission,
    #[serde(default)]
    pub read_only: bool,
}

impl NewKey {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() || self.name.len() > 128 {
            return Err("Key names must have 1..128 UTF-8 bytes and cannot be blank".into());
        }
        if self.read_only && self.permission != Permission::Secret {
            return Err("Only secret keys support read-only access".into());
        }
        Ok(())
    }
}

pub fn normalize_origins(origins: &mut Vec<String>) -> Result<(), String> {
    for origin in origins.iter() {
        crate::config::validate_origin(origin)?;
    }
    origins.sort();
    origins.dedup();
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    #[serde(default)]
    pub deleted: bool,
    pub id: String,
    pub name: String,
    pub keys: Vec<Credential>,
    #[serde(default)]
    pub settings: Settings,
    /// Events received before this time expired under an earlier retention
    /// policy. They stay hidden, and are deleted, even if retention grows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired_before_ms: Option<i64>,
}

pub fn primary_project(public_key: &str, secret_key: &str) -> Project {
    Project {
        deleted: false,
        id: default_project(),
        name: "Default project".into(),
        settings: Settings::default(),
        expired_before_ms: None,
        keys: vec![
            Credential {
                id: "primary-public".into(),
                name: "Primary public".into(),
                permission: Permission::Public,
                read_only: false,
                hash: hash(public_key),
            },
            Credential {
                id: "primary-secret".into(),
                name: "Primary secret".into(),
                permission: Permission::Secret,
                read_only: false,
                hash: hash(secret_key),
            },
        ],
    }
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

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.chars().count() > 128 {
        return Err("Project names must have 1..128 characters".into());
    }
    Ok(())
}

pub fn validate(projects: &[Project]) -> Result<(), String> {
    if projects.is_empty()
        || projects.iter().filter(|project| !project.deleted).count() > MAX_PROJECTS
        || !projects.iter().any(|p| p.id == DEFAULT_PROJECT)
    {
        return Err("Expected default and at most 1024 active projects".into());
    }
    let mut ids = HashSet::new();
    let mut hashes = HashSet::new();
    for project in projects {
        if project.deleted && !project.keys.is_empty() {
            return Err("Deleted projects cannot have keys".into());
        }
        validate_id(&project.id)?;
        validate_name(&project.name)?;
        if !ids.insert(&project.id) {
            return Err("Duplicate project ID".into());
        }
        let mut key_ids = HashSet::new();
        if project.keys.len() > MAX_KEYS {
            return Err("At most 64 keys per project".into());
        }
        for key in &project.keys {
            if key.read_only && key.permission != Permission::Secret {
                return Err("Only secret keys support read-only access".into());
            }
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
        validate_retention(project.settings.retention_seconds)?;
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
            return Err(ApiError::new(
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
    pub limits: crate::limits::Project,
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
    fn new(project: &Project, config: &Config, count: usize) -> Self {
        let queue_bytes = project
            .settings
            .queue_bytes
            .unwrap_or(config.batch.queue_bytes / count)
            .min(config.batch.queue_bytes);
        Self {
            id: project.id.clone(),
            limits: crate::limits::Project::new(&config.rate_limits),
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
        }
    }

    pub fn allows_origin(&self, origin: &str) -> bool {
        self.origins
            .read()
            .expect("origins lock")
            .iter()
            .any(|value| value == origin)
    }

    pub fn admit(&self, bytes: usize) -> Result<Admission, ApiError> {
        if bytes > self.queue_bytes {
            return Err(ApiError::new(
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
    projects: RwLock<HashMap<String, Arc<Access>>>,
    credentials: RwLock<HashMap<String, (Permission, Resolved)>>,
}

#[derive(Clone)]
pub struct Resolved {
    pub project: Arc<Access>,
    pub key_id: String,
    pub read_only: bool,
}

impl Resolved {
    pub fn require_write(&self) -> Result<(), ApiError> {
        if self.read_only {
            return Err(ApiError::new(
                axum::http::StatusCode::FORBIDDEN,
                "Read-only keys cannot manage projects or keys".into(),
            ));
        }
        Ok(())
    }
}

impl Registry {
    pub fn access(&self, id: &str) -> Result<Arc<Access>, ApiError> {
        self.projects
            .read()
            .map_err(|_| ApiError::internal())?
            .get(id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "Project is no longer available".into(),
                )
            })
    }
    pub fn export_fingerprint(&self, owner: &Resolved) -> Option<String> {
        self.credentials
            .read()
            .ok()?
            .iter()
            .find_map(|(hash, (permission, resolved))| {
                (*permission == Permission::Secret
                    && resolved.key_id == owner.key_id
                    && Arc::ptr_eq(&resolved.project, &owner.project))
                .then(|| hash.clone())
            })
    }

    pub fn export_owner(&self, fingerprint: &str) -> Option<Resolved> {
        self.credentials
            .read()
            .ok()?
            .get(fingerprint)
            .filter(|(permission, _)| *permission == Permission::Secret)
            .map(|(_, owner)| owner.clone())
    }

    pub fn new(keys: &Keys, config: &Config) -> Result<Self, String> {
        let projects = &keys.projects;
        validate(projects)?;
        let registry = Self {
            retention: RwLock::new(crate::retention::Policy::new(
                config.retention_seconds,
                projects,
            )),
            projects: RwLock::new(HashMap::new()),
            credentials: RwLock::new(HashMap::new()),
        };
        registry.replace_keys(keys, config);
        Ok(registry)
    }

    // Preserve existing access state and in-flight admissions when adding projects
    // or rotating keys. New projects receive a share of the installation limits.
    pub fn replace_keys(&self, keys: &Keys, config: &Config) {
        let next = keys.projects.clone();
        let count = next.iter().filter(|project| !project.deleted).count();
        let mut projects = self.projects.write().expect("projects lock");
        let mut credentials = HashMap::new();
        let mut active = HashSet::new();
        for project in next.into_iter().filter(|project| !project.deleted) {
            active.insert(project.id.clone());
            let access = projects
                .entry(project.id.clone())
                .or_insert_with(|| Arc::new(Access::new(&project, config, count)));
            for key in project.keys {
                credentials.insert(
                    key.hash,
                    (
                        key.permission,
                        Resolved {
                            project: access.clone(),
                            key_id: key.id,
                            read_only: key.read_only,
                        },
                    ),
                );
            }
        }
        projects.retain(|id, _| active.contains(id));
        *self.credentials.write().expect("credentials lock") = credentials;
    }

    pub fn allows_origin(&self, origin: &str) -> bool {
        self.projects
            .read()
            .expect("projects lock")
            .values()
            .any(|project| project.allows_origin(origin))
    }

    pub fn replace_origins(&self, project: &str, origins: Vec<String>) {
        *self.projects.read().expect("projects lock")[project]
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
                ApiError::new(
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

fn active_project_mut<'a>(
    projects: &'a mut [Project],
    id: &str,
) -> Result<&'a mut Project, &'static str> {
    projects
        .iter_mut()
        .find(|project| project.id == id && !project.deleted)
        .ok_or("Unknown project")
}

pub fn apply_command(
    keys: &mut Keys,
    command: &str,
    args: &[String],
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut list = keys.projects.clone();
    // Offline changes apply on startup; live administration clears this after
    // persisting the changes and updating the running registry.
    let mut output = serde_json::json!({"restart_required": true});
    match (command, args) {
            ("project-list", []) => {
                return Ok(serde_json::json!(list.iter().map(|p| serde_json::json!({"id": p.id, "name": p.name, "deleted": p.deleted, "settings": p.settings,
                    "keys": p.keys.iter().map(Credential::metadata).collect::<Vec<_>>() })).collect::<Vec<_>>()));
            }
            ("rotate-key", [kind]) => {
                let permission = Permission::parse(kind)?;
                let (mut key, secret) = credential(&format!("Primary {kind}"), permission);
                key.id = format!("primary-{kind}");
                output = serde_json::json!({"project_id": DEFAULT_PROJECT, "key_id": key.id, "key": secret,
                    "permission": permission, "read_only": false, "restart_required": true});
                let project = active_project_mut(&mut list, DEFAULT_PROJECT)?;
                project.keys.retain(|k| k.id != key.id);
                project.keys.push(key);
            }
            ("project-create", [name]) | ("project-create", [name, _]) | ("project-create", [name, _, _]) => {
                let id = uuid::Uuid::new_v4().to_string();
                let (ingest, public_key) = credential("Primary public", Permission::Public);
                let (project_key, secret_key) = credential("Primary secret", Permission::Secret);
                let mut settings = Settings::default();
                if let Some(origins) = args.get(1) {
                    settings.allowed_origins = Some(serde_json::from_str(origins)?);
                }
                if let Some(seconds) = args.get(2) {
                    settings.retention_seconds = serde_json::from_str(seconds)?;
                }
                output = serde_json::json!({"project_id": id, "name": name, "public_key": public_key, "secret_key": secret_key,
                    "retention_seconds": settings.retention_seconds, "restart_required": true});
                list.push(Project { id, name: name.clone(), settings, keys: vec![ingest, project_key], deleted: false, expired_before_ms: None });
            }
            ("project-rename", [id, name]) => {
                let project = active_project_mut(&mut list, id)?;
                project.name = name.clone();
            }
            ("project-retention", [id, value]) => {
                let seconds = if value == "inherit" { None } else {
                    Some(value.parse::<u64>().map_err(|_| "Expected retention seconds as a non-negative whole number, or inherit")?)
                };
                let project = active_project_mut(&mut list, id)?;
                project.settings.retention_seconds = seconds;
                output = serde_json::json!({"project_id": id, "retention_seconds": seconds, "restart_required": true});
            }
            ("project-delete", [id]) => {
                let project = active_project_mut(&mut list, id)?;
                project.deleted = true;
                project.keys.clear();
                // Keep the project and its retention override so stored events expire as before.
                output = serde_json::json!({"project_id": id, "deleted": true, "restart_required": true});
            }
            ("project-update", [id, value]) => {
                let mut update: Update = serde_json::from_str(value)?;
                update.validate()?;
                let project = active_project_mut(&mut list, id)?;
                if let Some(name) = update.name { project.name = name; }
                if let Some(origins) = update.allowed_origins { project.settings.allowed_origins = Some(origins); }
                if let Some(seconds) = update.retention_seconds { project.settings.retention_seconds = seconds; }
                output = serde_json::json!({"project_id": id, "name": project.name, "allowed_origins": project.settings.allowed_origins.clone().unwrap_or_default(),
                    "retention_seconds": project.settings.retention_seconds, "restart_required": true});
            }
            ("project-configure", [id, file]) => {
                let project = active_project_mut(&mut list, id)?;
                project.settings = serde_json::from_slice(&std::fs::read(file)?)?;
            }
            ("key-create", [id, kind, name]) | ("key-create", [id, kind, name, _]) => {
                let read_only = match args.get(3).map(String::as_str) {
                    None => false,
                    Some("read-only") => true,
                    _ => return Err("Expected read-only key option".into()),
                };
                let permission = Permission::parse(kind)?;
                if read_only && permission != Permission::Secret {
                    return Err("Only secret keys support read-only access".into());
                }
                let project = active_project_mut(&mut list, id)?;
                let (mut key, secret) = credential(name, permission);
                key.read_only = read_only;
                output = serde_json::json!({"project_id": id, "key_id": key.id, "key": secret, "permission": key.permission, "read_only": key.read_only, "restart_required": true});
                project.keys.push(key);
            }
            ("key-revoke", [id, key_id]) => {
                let project = active_project_mut(&mut list, id)?;
                let index = project.keys.iter().position(|k| &k.id == key_id).ok_or("Unknown key")?;
                project.keys.remove(index);
            }
            ("key-rotate", [id, key_id]) => {
                let project = active_project_mut(&mut list, id)?;
                let old = project.keys.iter_mut().find(|k| &k.id == key_id).ok_or("Unknown key")?;
                let (new, secret) = credential(&old.name, old.permission);
                old.hash = new.hash;
                output = serde_json::json!({"project_id": id, "key_id": key_id, "key": secret, "permission": old.permission, "read_only": old.read_only, "restart_required": true});
            }
            _ => return Err("Expected project list | project create <name> | project rename <id> <name> | project configure <id> <settings.json> | project retention <id> <seconds|inherit> | key create <project> <public|secret> <name> [--read-only] | key rotate <project> <key-id> | key revoke <project> <key-id>".into()),
        }
    validate(&list)?;
    keys.projects = list;
    Ok(output)
}
