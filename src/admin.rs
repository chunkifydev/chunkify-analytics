//! Project administration through HTTP and the CLI. Every change is a
//! compare-and-set of the project registry in ClickHouse, so commands and any
//! number of running servers can change projects concurrently.
use crate::{
    config::{Config, Keys},
    projects::{self, Registry},
};
use serde_json::Value;
use std::sync::{Arc, Mutex};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Request {
    command: String,
    args: Vec<String>,
}

// Validation and persistence finish before updating the active configuration.
// Retention updates wait for any current sweep before saving and acknowledging.
// When another server or command saved the registry first, the command is
// applied again to the newest registry, which also validates it again.
fn execute(
    config: &Config,
    keys: &mut Keys,
    registry: Option<&Registry>,
    request: &Request,
) -> Result<Value> {
    let mut attempts = 0;
    loop {
        match execute_once(config, keys, registry, request) {
            Err(error) if crate::clickhouse::is_conflict(&*error) && attempts < 5 => {
                attempts += 1;
                let latest = config.keys()?;
                if let Some(registry) = registry {
                    adopt(config, registry, &latest)?;
                }
                *keys = latest;
            }
            result => return result,
        }
    }
}

fn execute_once(
    config: &Config,
    keys: &mut Keys,
    registry: Option<&Registry>,
    request: &Request,
) -> Result<Value> {
    let mut next = keys.clone();
    let mut output = projects::apply_command(&mut next, &request.command, &request.args)?;
    record_expiration(config, keys, &mut next);
    if request.command == "project-list" {
        for project in output.as_array_mut().expect("project list") {
            project["effective_retention_seconds"] = project["settings"]["retention_seconds"]
                .as_u64()
                .unwrap_or(config.retention_seconds)
                .into();
        }
    }
    let changes_retention = matches!(
        request.command.as_str(),
        "project-retention" | "project-create" | "project-update"
    );
    if changes_retention {
        output["effective_retention_seconds"] = output["retention_seconds"]
            .as_u64()
            .unwrap_or(config.retention_seconds)
            .into();
    }
    if request.command == "project-update" {
        let origins: Vec<String> = serde_json::from_value(output["allowed_origins"].clone())?;
        output["effective_allowed_origins"] = serde_json::json!(projects::effective_origins(
            &config.allowed_origins,
            Some(&origins)
        ));
    }
    if request.command != "project-list" {
        // Save before touching the in-memory policy: the save is a network
        // round trip, and readers take the policy lock on every request.
        config.save_keys(&next)?;
        if changes_retention && let Some(registry) = registry {
            let policy = crate::retention::Policy::new(config.retention_seconds, &next.projects);
            *registry
                .retention
                .write()
                .map_err(|_| "Retention policy lock failed")? = policy;
        }
        if let Some(registry) = registry {
            if request.command == "project-update" {
                registry.replace_origins(
                    &request.args[0],
                    serde_json::from_value(output["effective_allowed_origins"].clone())?,
                );
            } else if request.command != "project-retention" {
                registry.replace_keys(&next, config);
            }
            output["restart_required"] = false.into();
        }
        *keys = next;
    }
    Ok(output)
}

/// When a project's effective retention changes, events already expired under
/// the previous policy stay expired: reads hid them and the sweep deletes them.
fn record_expiration(config: &Config, previous: &Keys, next: &mut Keys) {
    let now = chrono::Utc::now().timestamp_millis();
    for project in &mut next.projects {
        let Some(before) = previous.projects.iter().find(|p| p.id == project.id) else {
            continue;
        };
        let old = before
            .settings
            .retention_seconds
            .unwrap_or(config.retention_seconds);
        let new = project
            .settings
            .retention_seconds
            .unwrap_or(config.retention_seconds);
        if old != new && old > 0 {
            let floor = now.saturating_sub((old * 1000) as i64);
            project.expired_before_ms =
                Some(project.expired_before_ms.map_or(floor, |f| f.max(floor)));
        }
    }
}

/// Make a newer registry active on a running server.
fn adopt(config: &Config, registry: &Registry, next: &Keys) -> Result<()> {
    let mut policy = registry
        .retention
        .write()
        .map_err(|_| "Retention policy lock failed")?;
    *policy = crate::retention::Policy::new(config.retention_seconds, &next.projects);
    registry.replace_keys(next, config);
    for project in next.projects.iter().filter(|project| !project.deleted) {
        registry.replace_origins(
            &project.id,
            projects::effective_origins(
                &config.allowed_origins,
                project.settings.allowed_origins.as_deref(),
            ),
        );
    }
    Ok(())
}

/// Minimum time between registry refreshes triggered by unknown credentials.
const MISS_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// How long a request with an unknown credential waits for such a refresh.
const MISS_REFRESH_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct Controller {
    config: Arc<Config>,
    registry: Arc<Registry>,
    keys: Mutex<Keys>,
    /// When the latest refresh after an unknown credential started.
    missed: tokio::sync::Mutex<Option<std::time::Instant>>,
}

pub enum ProjectAction {
    Update(projects::Update),
    Delete,
    Keys(KeyAction),
}

pub enum KeyAction {
    List,
    Rotate(String),
    Create(projects::NewKey),
}

impl Controller {
    pub fn authorize_admin(&self, secret: &str) -> std::result::Result<(), crate::api::ApiError> {
        use crate::api::ApiError;
        let expected = self.config.admin_key_hash.as_ref().ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "Project administration is disabled".into(),
            )
        })?;
        if *expected != projects::hash(secret) {
            return Err(ApiError::new(
                axum::http::StatusCode::UNAUTHORIZED,
                "Missing or invalid admin key".into(),
            ));
        }
        Ok(())
    }

    /// Adopt a newer project registry saved by the CLI or another server.
    pub fn sync(&self) -> Result<()> {
        let settings = &self.config.clickhouse;
        // `serve` created the schema at startup; refreshes only read the registry.
        let Some((version, bytes)) = crate::clickhouse::read_registry(settings)? else {
            return Ok(());
        };
        let mut keys = self.keys.lock().map_err(|_| "Administration lock failed")?;
        if version <= settings.loaded_version() {
            return Ok(());
        }
        let next: Keys = serde_json::from_slice(&bytes)?;
        projects::validate(&next.projects)?;
        adopt(&self.config, &self.registry, &next)?;
        *keys = next;
        settings.set_loaded_version(version);
        Ok(())
    }

    /// Refresh after an unknown credential, so a key created through the CLI or
    /// another server works here at once. Anyone can send unknown keys, so
    /// refreshes run one at a time and at most once per `MISS_REFRESH_INTERVAL`.
    /// Waiting requests queue as tasks, not threads, and are answered by any
    /// refresh that started after they arrived. A request gives up after
    /// `MISS_REFRESH_WAIT`.
    pub async fn sync_after_miss(self: &Arc<Self>, arrived: std::time::Instant) -> bool {
        let refresh = async {
            let mut started = self.missed.lock().await;
            if started.is_some_and(|at| at >= arrived) {
                return true;
            }
            if let Some(at) = *started {
                tokio::time::sleep(MISS_REFRESH_INTERVAL.saturating_sub(at.elapsed())).await;
            }
            *started = Some(std::time::Instant::now());
            let controller = self.clone();
            tokio::task::spawn_blocking(move || controller.sync().is_ok())
                .await
                .unwrap_or(false)
        };
        tokio::time::timeout(MISS_REFRESH_WAIT, refresh)
            .await
            .unwrap_or(false)
    }

    // Mutations start from the newest shared registry to avoid stale writes.
    fn sync_before_change(&self) -> std::result::Result<(), crate::api::ApiError> {
        self.sync().map_err(|_| crate::api::ApiError::unavailable())
    }

    pub fn create_project(
        &self,
        secret: &str,
        name: String,
        origins: Vec<String>,
        retention_seconds: Option<u64>,
    ) -> std::result::Result<Value, crate::api::ApiError> {
        use crate::api::ApiError;
        self.authorize_admin(secret)?;
        self.sync_before_change()?;
        projects::validate_retention(retention_seconds).map_err(ApiError::bad)?;
        let mut keys = self.keys.lock().map_err(|_| ApiError::unavailable())?;
        if keys
            .projects
            .iter()
            .filter(|project| !project.deleted)
            .count()
            >= projects::MAX_PROJECTS
        {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "Project limit reached".into(),
            ));
        }
        let request = Request {
            command: "project-create".into(),
            args: vec![
                name,
                serde_json::to_string(&origins).map_err(|_| ApiError::internal())?,
                serde_json::to_string(&retention_seconds).map_err(|_| ApiError::internal())?,
            ],
        };
        execute(&self.config, &mut keys, Some(&self.registry), &request)
            .map_err(|_| ApiError::internal())
    }

    pub fn new(config: Arc<Config>, registry: Arc<Registry>, keys: Keys) -> Self {
        Self {
            config,
            registry,
            keys: Mutex::new(keys),
            missed: tokio::sync::Mutex::new(None),
        }
    }

    pub fn project(
        &self,
        secret: &str,
        action: ProjectAction,
    ) -> std::result::Result<Value, crate::api::ApiError> {
        use crate::{api::ApiError, projects::Permission};
        self.sync_before_change()?;
        let mut keys = self.keys.lock().map_err(|_| ApiError::unavailable())?;
        // Recheck under the mutation lock so a revoked or rotated token cannot
        // authorize another queued change.
        let resolved = self.registry.authorize(secret, Permission::Secret)?;
        resolved.require_write()?;
        self.apply_project_action(&mut keys, &resolved.project.id, action)
    }

    pub fn admin_keys(
        &self,
        secret: &str,
        id: &str,
        action: KeyAction,
    ) -> std::result::Result<Value, crate::api::ApiError> {
        self.authorize_admin(secret)?;
        self.sync_before_change()?;
        let mut keys = self
            .keys
            .lock()
            .map_err(|_| crate::api::ApiError::unavailable())?;
        self.apply_project_action(&mut keys, id, ProjectAction::Keys(action))
    }

    fn apply_project_action(
        &self,
        keys: &mut Keys,
        id: &str,
        action: ProjectAction,
    ) -> std::result::Result<Value, crate::api::ApiError> {
        use crate::api::ApiError;
        let project = keys
            .projects
            .iter()
            .find(|project| project.id == id && !project.deleted)
            .ok_or_else(|| {
                ApiError::new(axum::http::StatusCode::NOT_FOUND, "Unknown project".into())
            })?;
        let request = match action {
            ProjectAction::Keys(KeyAction::List) => {
                return Ok(
                    serde_json::json!({"project_id": id, "keys": project.keys.iter().map(projects::Credential::metadata).collect::<Vec<_>>() }),
                );
            }
            ProjectAction::Keys(KeyAction::Create(key)) => {
                key.validate().map_err(ApiError::bad)?;
                if project.keys.len() >= projects::MAX_KEYS {
                    return Err(ApiError::new(
                        axum::http::StatusCode::CONFLICT,
                        "Project key limit reached".into(),
                    ));
                }
                let mut args = vec![
                    id.into(),
                    match key.permission {
                        projects::Permission::Public => "public".into(),
                        projects::Permission::Secret => "secret".into(),
                    },
                    key.name,
                ];
                if key.read_only {
                    args.push("read-only".into());
                }
                Request {
                    command: "key-create".into(),
                    args,
                }
            }
            ProjectAction::Delete => Request {
                command: "project-delete".into(),
                args: vec![id.into()],
            },
            ProjectAction::Update(update) => Request {
                command: "project-update".into(),
                args: vec![
                    id.into(),
                    serde_json::to_string(&update).map_err(|_| ApiError::internal())?,
                ],
            },
            ProjectAction::Keys(KeyAction::Rotate(key)) => {
                if !project.keys.iter().any(|credential| credential.id == key) {
                    return Err(ApiError::new(
                        axum::http::StatusCode::NOT_FOUND,
                        "Unknown project key".into(),
                    ));
                }
                Request {
                    command: "key-rotate".into(),
                    args: vec![id.into(), key],
                }
            }
        };
        execute(&self.config, keys, Some(&self.registry), &request)
            .map_err(|_| ApiError::internal())
    }
}

/// Poll the shared ClickHouse registry so changes made through any analytics
/// server, or the CLI, apply to this server without a restart.
pub fn refresh_registry(
    config: Arc<Config>,
    controller: Arc<Controller>,
) -> tokio::task::JoinHandle<()> {
    let interval = config.clickhouse.registry_refresh;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let controller = controller.clone();
            match tokio::task::spawn_blocking(move || controller.sync().map_err(|e| e.to_string()))
                .await
            {
                Ok(Ok(())) => {}
                _ => eprintln!("Project registry refresh failed; will retry"),
            }
        }
    })
}

/// Apply a CLI command to the shared registry. Running servers adopt the change
/// at their next refresh, except ingestion limits, which apply at startup.
pub fn command(config: &Config, command: &str, args: &[String]) -> Result<()> {
    let request = Request {
        command: command.into(),
        args: args.to_vec(),
    };
    let mut output = execute(config, &mut config.keys()?, None, &request)?;
    if let Some(object) = output.as_object_mut()
        && object.contains_key("restart_required")
    {
        object.insert(
            "restart_required".into(),
            (command == "project-configure").into(),
        );
    }
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        config: Arc<Config>,
        keys: Keys,
        public_key: String,
        secret_key: String,
        registry: Arc<Registry>,
    }
    impl Fixture {
        fn new() -> Self {
            let config = crate::clickhouse::testing::config("");
            let generated = config.initialize().unwrap();
            let keys = config.keys().unwrap();
            let registry = Arc::new(Registry::new(&keys, &config).unwrap());
            Self {
                public_key: generated["public_key"].as_str().unwrap().into(),
                secret_key: generated["secret_key"].as_str().unwrap().into(),
                config: Arc::new(config),
                keys,
                registry,
            }
        }
        fn saved(&self) -> (u64, Vec<u8>) {
            crate::clickhouse::load_registry(&self.config.clickhouse)
                .unwrap()
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            crate::clickhouse::testing::drop_database(&self.config.clickhouse);
        }
    }
    fn request(command: &str, args: &[&str]) -> Request {
        Request {
            command: command.into(),
            args: args.iter().map(|value| (*value).into()).collect(),
        }
    }

    #[test]
    fn controller_rejects_read_only_keys_before_project_or_key_management() {
        let mut fixture = Fixture::new();
        let readonly = execute(
            &fixture.config,
            &mut fixture.keys,
            Some(&fixture.registry),
            &request("key-create", &["default", "secret", "MCP", "read-only"]),
        )
        .unwrap();
        assert_eq!(readonly["restart_required"], false);
        let token = readonly["key"].as_str().unwrap();
        let resolved = fixture
            .registry
            .authorize(token, projects::Permission::Secret)
            .ok()
            .unwrap();
        assert!(resolved.read_only);
        let controller = Controller::new(
            fixture.config.clone(),
            fixture.registry.clone(),
            fixture.keys.clone(),
        );
        let before = fixture.saved();
        for action in [
            ProjectAction::Keys(KeyAction::List),
            ProjectAction::Update(
                serde_json::from_value(serde_json::json!({"name": "Forbidden"})).unwrap(),
            ),
            ProjectAction::Keys(KeyAction::Rotate(
                readonly["key_id"].as_str().unwrap().into(),
            )),
            ProjectAction::Delete,
        ] {
            let error = controller.project(token, action).err().unwrap();
            assert_eq!(error.status, axum::http::StatusCode::FORBIDDEN);
        }
        assert_eq!(fixture.saved(), before);
        assert!(
            controller
                .project(&fixture.secret_key, ProjectAction::Keys(KeyAction::List))
                .is_ok()
        );
    }

    #[test]
    fn controller_requires_admin_authorization_for_key_recovery() {
        let mut fixture = Fixture::new();
        let admin_key = "installation-admin";
        Arc::get_mut(&mut fixture.config).unwrap().admin_key_hash = Some(projects::hash(admin_key));
        let controller = Controller::new(
            fixture.config.clone(),
            fixture.registry.clone(),
            fixture.keys.clone(),
        );
        let before = fixture.saved();
        for token in ["", "invalid", &fixture.secret_key, &fixture.public_key] {
            for action in [KeyAction::List, KeyAction::Rotate("primary-secret".into())] {
                let error = controller
                    .admin_keys(token, "default", action)
                    .err()
                    .unwrap();
                assert_eq!(error.status, axum::http::StatusCode::UNAUTHORIZED);
            }
        }
        assert_eq!(fixture.saved(), before);
        let recovered = controller
            .admin_keys(
                admin_key,
                "default",
                KeyAction::Rotate("primary-secret".into()),
            )
            .unwrap();
        assert_eq!(recovered["key_id"], "primary-secret");
        assert_eq!(recovered["read_only"], false);
        assert!(
            controller
                .project(&fixture.secret_key, ProjectAction::Keys(KeyAction::List))
                .is_err()
        );
        assert!(
            controller
                .project(
                    recovered["key"].as_str().unwrap(),
                    ProjectAction::Keys(KeyAction::List)
                )
                .is_ok()
        );
        let saved = String::from_utf8(fixture.saved().1).unwrap();
        assert!(!saved.contains(recovered["key"].as_str().unwrap()));
        assert!(saved.contains(&projects::hash(recovered["key"].as_str().unwrap())));
    }

    #[test]
    fn key_changes_preserve_in_flight_admissions_and_access_state() {
        let mut fixture = Fixture::new();
        let access = fixture
            .registry
            .authorize(&fixture.public_key, projects::Permission::Public)
            .ok()
            .unwrap()
            .project;
        let permit = access.admit(access.queue_bytes).ok().unwrap();
        let created = execute(
            &fixture.config,
            &mut fixture.keys,
            Some(&fixture.registry),
            &request("project-create", &["New customer"]),
        )
        .unwrap();
        assert_eq!(created["restart_required"], false);
        let customer = fixture
            .registry
            .authorize(
                created["public_key"].as_str().unwrap(),
                projects::Permission::Public,
            )
            .ok()
            .unwrap();
        assert_eq!(customer.project.id, created["project_id"]);
        let output = execute(
            &fixture.config,
            &mut fixture.keys,
            Some(&fixture.registry),
            &request("key-rotate", &["default", "primary-public"]),
        )
        .unwrap();
        let updated = fixture
            .registry
            .authorize(
                output["key"].as_str().unwrap(),
                projects::Permission::Public,
            )
            .ok()
            .unwrap()
            .project;
        assert!(Arc::ptr_eq(&access, &updated));
        assert!(updated.admit(1).is_err());
        drop(permit);
        assert!(updated.admit(1).is_ok());
    }

    #[test]
    fn deletion_revokes_default_keys_and_keeps_project_expiration() {
        let mut fixture = Fixture::new();
        Arc::get_mut(&mut fixture.config).unwrap().retention_seconds = 0;
        execute(
            &fixture.config,
            &mut fixture.keys,
            Some(&fixture.registry),
            &request("project-retention", &["default", "10"]),
        )
        .unwrap();
        let controller = Controller::new(
            fixture.config.clone(),
            fixture.registry.clone(),
            fixture.keys.clone(),
        );
        let result = controller
            .project(&fixture.secret_key, ProjectAction::Delete)
            .ok()
            .unwrap();
        assert_eq!(result["deleted"], true);
        assert_eq!(result["restart_required"], false);
        for (key, permission) in [
            (&fixture.public_key, projects::Permission::Public),
            (&fixture.secret_key, projects::Permission::Secret),
        ] {
            assert!(fixture.registry.authorize(key, permission).is_err());
        }
        let saved = fixture.config.keys().unwrap();
        assert!(saved.projects[0].deleted);
        assert!(saved.projects[0].keys.is_empty());
        let restored = Registry::new(&saved, &fixture.config).unwrap();
        assert!(
            restored
                .authorize(&fixture.secret_key, projects::Permission::Secret)
                .is_err()
        );
        // Stored events of the deleted project still expire with its own policy.
        let policy = restored.retention.read().unwrap();
        assert_eq!(policy.project_cutoff("default", 11000), 1000);
        drop(policy);
        assert!(
            controller
                .project(&fixture.secret_key, ProjectAction::Delete)
                .is_err()
        );
    }

    #[test]
    fn a_stale_change_is_reapplied_on_top_of_a_concurrent_change() {
        let mut fixture = Fixture::new();
        let (version, _) = fixture.saved();
        // Another server creates a key first, from the same registry version.
        let mut other_keys = fixture.keys.clone();
        let mut other_config = crate::clickhouse::testing::config("");
        other_config.clickhouse = fixture.config.clickhouse.clone();
        other_config.clickhouse.registry_version =
            Arc::new(std::sync::atomic::AtomicU64::new(version));
        let other = execute(
            &other_config,
            &mut other_keys,
            None,
            &request("key-create", &["default", "secret", "Elsewhere"]),
        )
        .unwrap();
        assert_eq!(fixture.saved().0, version + 1);
        // This server's change, based on the older version, is applied again.
        let rotated = execute(
            &fixture.config,
            &mut fixture.keys,
            Some(&fixture.registry),
            &request("key-rotate", &["default", "primary-public"]),
        )
        .unwrap();
        assert_eq!(fixture.saved().0, version + 2);
        let saved = fixture.config.keys().unwrap();
        let ids: Vec<_> = saved.projects[0]
            .keys
            .iter()
            .map(|key| key.id.as_str())
            .collect();
        assert!(ids.contains(&other["key_id"].as_str().unwrap()));
        for (key, works) in [
            (other["key"].as_str().unwrap(), true),
            (rotated["key"].as_str().unwrap(), true),
            (fixture.public_key.as_str(), false),
        ] {
            let permission = if key.starts_with("ck_public") {
                projects::Permission::Public
            } else {
                projects::Permission::Secret
            };
            assert_eq!(fixture.registry.authorize(key, permission).is_ok(), works);
        }
        // A change that is invalid on the newest registry is not saved.
        let mut stale = fixture.keys.clone();
        stale.projects[0]
            .keys
            .retain(|key| key.id != "primary-secret");
        fixture
            .config
            .clickhouse
            .registry_version
            .store(version, std::sync::atomic::Ordering::Release);
        assert!(
            execute(
                &fixture.config,
                &mut stale,
                None,
                &request("key-revoke", &["default", "missing"]),
            )
            .is_err()
        );
        assert_eq!(fixture.saved().0, version + 2);
    }
}
