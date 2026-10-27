//! Local administration uses the data-directory lock when stopped, or the
//! running collector's private socket. It never opens a second database writer.
use crate::{
    config::{Config, Keys},
    projects::{self, Registry},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    command: String,
    args: Vec<String>,
}

type Reply = std::result::Result<Value, String>;

fn live_command(command: &str) -> bool {
    matches!(
        command,
        "project-list"
            | "project-create"
            | "project-retention"
            | "project-update"
            | "project-delete"
            | "key-create"
            | "key-rotate"
            | "key-revoke"
            | "rotate-key"
    )
}

// Validation and persistence finish before updating the active configuration.
// Retention updates wait for any current sweep before saving and acknowledging.
fn execute(
    config: &Config,
    keys: &mut Keys,
    registry: Option<&Registry>,
    request: &Request,
) -> Result<Value> {
    if registry.is_some() && !live_command(&request.command) {
        return Err("Stop the collector before changing projects or project settings.".into());
    }
    let mut next = keys.clone();
    let mut output = projects::apply_command(&mut next, &request.command, &request.args)?;
    if request.command == "project-list" {
        for project in output.as_array_mut().expect("project list") {
            project["effective_retention_seconds"] = project["settings"]["retention_seconds"]
                .as_u64()
                .unwrap_or(config.retention_seconds)
                .into();
        }
    }
    if request.command == "project-retention" {
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
        let mut retention = if request.command == "project-retention" {
            registry
                .map(|registry| {
                    registry
                        .retention
                        .write()
                        .map_err(|_| "Retention policy lock failed")
                })
                .transpose()?
        } else {
            None
        };
        let warning = config.save_keys(&next)?;
        if let Some(policy) = retention.as_mut() {
            **policy =
                crate::retention::Policy::new(config.retention_seconds, &projects::projects(&next));
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
        if let Some(warning) = warning {
            output["warning"] = warning.into();
        }
    }
    Ok(output)
}

pub struct Controller {
    config: Arc<Config>,
    registry: Arc<Registry>,
    keys: Mutex<Keys>,
}

pub enum ProjectAction {
    Update(projects::Update),
    Delete,
    Keys,
    RotateKey(String),
}

impl Controller {
    pub fn authorize_admin(&self, secret: &str) -> std::result::Result<(), crate::api::ApiError> {
        use crate::api::ApiError;
        let expected = self.config.admin_key_hash.as_ref().ok_or_else(|| {
            ApiError(
                axum::http::StatusCode::NOT_FOUND,
                "Project creation is disabled".into(),
            )
        })?;
        if *expected != projects::hash(secret) {
            return Err(ApiError(
                axum::http::StatusCode::UNAUTHORIZED,
                "Missing or invalid admin key".into(),
            ));
        }
        Ok(())
    }

    pub fn create_project(
        &self,
        secret: &str,
        name: String,
        origins: Vec<String>,
    ) -> std::result::Result<Value, crate::api::ApiError> {
        use crate::api::ApiError;
        self.authorize_admin(secret)?;
        let mut keys = self.keys.lock().map_err(|_| ApiError::unavailable())?;
        if projects::projects(&keys)
            .iter()
            .filter(|project| !project.deleted)
            .count()
            >= projects::MAX_PROJECTS
        {
            return Err(ApiError(
                axum::http::StatusCode::CONFLICT,
                "Project limit reached".into(),
            ));
        }
        let request = Request {
            command: "project-create".into(),
            args: vec![
                name,
                serde_json::to_string(&origins).map_err(|_| ApiError::internal())?,
            ],
        };
        execute(&self.config, &mut keys, Some(&self.registry), &request)
            .map_err(|_| ApiError::internal())
    }

    fn new(config: Arc<Config>, registry: Arc<Registry>, keys: Keys) -> Self {
        Self {
            config,
            registry,
            keys: Mutex::new(keys),
        }
    }

    fn execute(&self, request: &Request) -> Reply {
        let mut keys = self.keys.lock().map_err(|_| "Administration lock failed")?;
        execute(&self.config, &mut keys, Some(&self.registry), request)
            .map_err(|error| error.to_string())
    }

    pub fn project(
        &self,
        secret: &str,
        action: ProjectAction,
    ) -> std::result::Result<Value, crate::api::ApiError> {
        use crate::{api::ApiError, projects::Permission};
        let mut keys = self.keys.lock().map_err(|_| ApiError::unavailable())?;
        // Recheck under the mutation lock so a revoked or rotated token cannot
        // authorize another queued change.
        let resolved = self.registry.authorize(secret, Permission::Secret)?;
        resolved.require_write()?;
        let id = resolved.project.id.clone();
        let project = projects::projects(&keys)
            .into_iter()
            .find(|project| project.id == id)
            .ok_or_else(ApiError::internal)?;
        let request = match action {
            ProjectAction::Keys => {
                return Ok(
                    serde_json::json!({"project_id": id, "keys": project.keys.iter().map(projects::Credential::metadata).collect::<Vec<_>>() }),
                );
            }
            ProjectAction::Delete => Request {
                command: "project-delete".into(),
                args: vec![id],
            },
            ProjectAction::Update(update) => Request {
                command: "project-update".into(),
                args: vec![
                    id,
                    serde_json::to_string(&update).map_err(|_| ApiError::internal())?,
                ],
            },
            ProjectAction::RotateKey(key) => {
                if !project.keys.iter().any(|credential| credential.id == key) {
                    return Err(ApiError(
                        axum::http::StatusCode::NOT_FOUND,
                        "Unknown project key".into(),
                    ));
                }
                Request {
                    command: "key-rotate".into(),
                    args: vec![id, key],
                }
            }
        };
        execute(&self.config, &mut keys, Some(&self.registry), &request)
            .map_err(|_| ApiError::internal())
    }
}

pub async fn command(config: &Config, command: &str, args: &[String]) -> Result<()> {
    let request = Request {
        command: command.into(),
        args: args.to_vec(),
    };
    let output = if let Some(_lock) = config.try_lock()? {
        execute(config, &mut config.keys()?, None, &request)?
    } else if live_command(command) {
        live::send(config, &request).await?
    } else {
        return Err("Stop the collector before changing projects or project settings.".into());
    };
    if command == "rotate-key" {
        let suffix = if output["restart_required"] == false {
            "The new key is active."
        } else {
            "Start the collector to use it."
        };
        println!(
            "Rotated {} key. Read the new value from {}. {suffix}",
            args[0],
            config.key_cache_path().display()
        );
        if let Some(warning) = output["warning"].as_str() {
            eprintln!("{warning}");
        }
    } else {
        println!("{output}");
    }
    Ok(())
}

pub use live::Server;

#[cfg(unix)]
mod live {
    use super::*;
    use std::{
        fs, io,
        os::unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        },
        path::PathBuf,
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{UnixListener, UnixStream},
        sync::oneshot,
        task::JoinHandle,
        time::timeout,
    };

    const REQUEST_LIMIT: u64 = 16 * 1024;
    const REPLY_LIMIT: u64 = 32 * 1024 * 1024;
    const IO_TIMEOUT: Duration = Duration::from_secs(5);

    fn socket_path(config: &Config, create: bool) -> Result<(PathBuf, Option<PathBuf>)> {
        let directory = fs::canonicalize(&config.data_dir)?;
        let path = directory.join("admin.sock");
        // Use the smaller macOS limit on both platforms. Resolve aliases first
        // so the server and CLI choose the same address for the same directory.
        if path.as_os_str().as_bytes().len() < 104 {
            return Ok((path, None));
        }
        // SAFETY: geteuid takes no pointers and only returns the process identity.
        let uid = unsafe { libc::geteuid() };
        let digest = ring::digest::digest(&ring::digest::SHA256, directory.as_os_str().as_bytes());
        let hash: String = digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let locator = directory.join("admin.socket-id");
        let runtime_for = |id: uuid::Uuid| {
            PathBuf::from(format!(
                "/tmp/ck-admin-{uid}-{}-{}",
                &hash[..32],
                id.simple()
            ))
        };
        let runtime = if create {
            // An unpredictable suffix prevents another local user from reserving
            // our runtime path before startup. The private data directory holds
            // the locator; the canonical-path hash keeps restored copies apart.
            if let Ok(previous) = fs::read_to_string(&locator)
                && let Ok(id) = uuid::Uuid::parse_str(&previous)
            {
                let stale = runtime_for(id);
                if fs::symlink_metadata(&stale)
                    .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o777 == 0o700)
                {
                    let socket = stale.join("s");
                    if fs::symlink_metadata(&socket)
                        .is_ok_and(|m| m.file_type().is_socket() && m.uid() == uid)
                    {
                        let _ = fs::remove_file(socket);
                    }
                    let _ = fs::remove_dir(stale);
                }
            }
            let id = uuid::Uuid::new_v4();
            let runtime = runtime_for(id);
            fs::DirBuilder::new().mode(0o700).create(&runtime)?;
            let temporary = directory.join(format!(".admin-{id}.tmp"));
            let saved = (|| -> io::Result<()> {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)?;
                file.write_all(id.to_string().as_bytes())?;
                drop(file);
                fs::rename(&temporary, &locator)
            })();
            if let Err(error) = saved {
                let _ = fs::remove_file(temporary);
                let _ = fs::remove_dir(runtime);
                return Err(error.into());
            }
            runtime
        } else {
            runtime_for(uuid::Uuid::parse_str(&fs::read_to_string(locator)?)?)
        };
        let metadata = fs::symlink_metadata(&runtime)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
            return Err("Unsafe local administration directory in /tmp; expected a private directory owned by the collector user.".into());
        }
        Ok((runtime.join("s"), Some(runtime)))
    }

    // Remove only the socket we bound, including on startup failure. A crashed
    // collector leaves a socket that the next exclusive lock owner can replace.
    struct SocketFile {
        path: PathBuf,
        inode: u64,
        runtime_directory: Option<PathBuf>,
    }
    impl Drop for SocketFile {
        fn drop(&mut self) {
            if fs::symlink_metadata(&self.path)
                .is_ok_and(|m| m.file_type().is_socket() && m.ino() == self.inode)
            {
                let _ = fs::remove_file(&self.path);
                if let Some(directory) = &self.runtime_directory {
                    let _ = fs::remove_dir(directory);
                }
            }
        }
    }

    pub struct Server {
        listener: UnixListener,
        socket: SocketFile,
        controller: Arc<Controller>,
        uid: u32,
    }

    impl Server {
        // Caller must hold the exclusive data-directory lock for this lifetime.
        pub fn bind(config: Arc<Config>, registry: Arc<Registry>, keys: Keys) -> Result<Self> {
            let (path, runtime_directory) = socket_path(&config, true)?;
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)?,
                Ok(_) => return Err("admin.sock already exists and is not a socket; move it before starting the collector.".into()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
            let listener = UnixListener::bind(&path)?;
            let metadata = fs::symlink_metadata(&path)?;
            let socket = SocketFile {
                path,
                inode: metadata.ino(),
                runtime_directory,
            };
            fs::set_permissions(&socket.path, fs::Permissions::from_mode(0o600))?;
            Ok(Self {
                listener,
                socket,
                controller: Arc::new(Controller::new(config, registry, keys)),
                uid: metadata.uid(),
            })
        }

        pub fn controller(&self) -> Arc<Controller> {
            self.controller.clone()
        }

        pub fn start(self) -> Running {
            let (stop, mut stopped) = oneshot::channel();
            let task = tokio::spawn(async move {
                let Self {
                    listener,
                    socket,
                    controller,
                    uid,
                } = self;
                loop {
                    let connection = tokio::select! {
                        biased;
                        _ = &mut stopped => break,
                        connection = listener.accept() => connection,
                    };
                    let Ok((mut stream, _)) = connection else {
                        eprintln!("Local administration listener failed");
                        break;
                    };
                    // macOS does not enforce socket file permissions for connect.
                    // Peer credentials also restrict access on a shared host.
                    if !stream
                        .peer_cred()
                        .is_ok_and(|peer| peer.uid() == uid || peer.uid() == 0)
                    {
                        continue;
                    }
                    let request =
                        timeout(IO_TIMEOUT, receive::<Request>(&mut stream, REQUEST_LIMIT)).await;
                    let reply = match request {
                        Ok(Ok(request)) => {
                            let controller = controller.clone();
                            let result =
                                tokio::task::spawn_blocking(move || controller.execute(&request))
                                    .await;
                            match result {
                                Ok(reply) => reply,
                                Err(_) => {
                                    eprintln!("Local administration worker failed");
                                    break;
                                }
                            }
                        }
                        _ => Err("Invalid or incomplete administration request".into()),
                    };
                    if let Ok(bytes) = serde_json::to_vec(&reply) {
                        let _ = timeout(IO_TIMEOUT, stream.write_all(&bytes)).await;
                    }
                }
                drop(listener);
                drop(socket);
            });
            Running { stop, task }
        }
    }

    pub struct Running {
        stop: oneshot::Sender<()>,
        task: JoinHandle<()>,
    }
    impl Running {
        pub async fn stop(self) {
            let _ = self.stop.send(());
            if self.task.await.is_err() {
                eprintln!("Local administration worker failed during shutdown");
            }
        }
    }

    async fn receive<T: serde::de::DeserializeOwned>(
        stream: &mut UnixStream,
        limit: u64,
    ) -> std::result::Result<T, String> {
        let mut bytes = Vec::new();
        stream
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "Administration connection failed")?;
        if bytes.len() as u64 > limit {
            return Err("Administration message is too large".into());
        }
        serde_json::from_slice(&bytes).map_err(|_| "Invalid administration message".into())
    }

    pub(super) async fn send(config: &Config, request: &Request) -> Result<Value> {
        let bytes = serde_json::to_vec(request)?;
        if bytes.len() as u64 > REQUEST_LIMIT {
            return Err("Administration request is too large".into());
        }
        let (path, _) = socket_path(config, false)?;
        let metadata = fs::symlink_metadata(&path).map_err(|_| "Data directory is busy and local administration is unavailable. Wait for startup/shutdown or stop the collector first.")?;
        if !metadata.file_type().is_socket() {
            return Err("admin.sock is not a socket".into());
        }
        let exchange = async {
            let mut stream = UnixStream::connect(path).await.map_err(|_| "Cannot connect to the running collector; wait for startup/shutdown or stop it first.")?;
            if stream
                .peer_cred()
                .map_err(|_| "Cannot verify administration server")?
                .uid()
                != metadata.uid()
            {
                return Err("Administration server owner does not match the socket".into());
            }
            stream
                .write_all(&bytes)
                .await
                .map_err(|_| "Administration request could not be sent")?;
            stream
                .shutdown()
                .await
                .map_err(|_| "Administration request could not be sent")?;
            receive::<Reply>(&mut stream, REPLY_LIMIT).await?
        };
        timeout(Duration::from_secs(15), exchange).await
            .map_err(|_| "Administration timed out; the change may have been applied. Check project list before retrying.")?
            .map_err(Into::into)
    }
}

#[cfg(not(unix))]
mod live {
    use super::*;
    pub struct Server {
        controller: Arc<Controller>,
    }
    impl Server {
        pub fn bind(config: Arc<Config>, registry: Arc<Registry>, keys: Keys) -> Result<Self> {
            Ok(Self {
                controller: Arc::new(Controller::new(config, registry, keys)),
            })
        }
        pub fn controller(&self) -> Arc<Controller> {
            self.controller.clone()
        }
        pub fn start(self) -> Self {
            self
        }
        pub async fn stop(self) {}
    }
    pub(super) async fn send(_: &Config, _: &Request) -> Result<Value> {
        Err("Live key administration requires Linux or macOS. Stop the collector before running this command.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    struct Fixture {
        config: Arc<Config>,
        keys: Keys,
        registry: Arc<Registry>,
        directory: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let mut config =
                Config::load_with(&crate::settings::Settings::default(), None, None).unwrap();
            config.data_dir = std::env::temp_dir().join(format!(
                "ck-admin-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ));
            config.initialize().unwrap();
            let keys = config.keys().unwrap();
            let registry = Arc::new(Registry::new(&keys, &config).unwrap());
            Self {
                directory: config.data_dir.clone(),
                config: Arc::new(config),
                keys,
                registry,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
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
        let before = fs::read(fixture.directory.join("keys.json")).unwrap();
        for action in [
            ProjectAction::Keys,
            ProjectAction::Update(
                serde_json::from_value(serde_json::json!({"name": "Forbidden"})).unwrap(),
            ),
            ProjectAction::RotateKey(readonly["key_id"].as_str().unwrap().into()),
            ProjectAction::Delete,
        ] {
            let error = controller.project(token, action).err().unwrap();
            assert_eq!(error.0, axum::http::StatusCode::FORBIDDEN);
        }
        assert_eq!(
            fs::read(fixture.directory.join("keys.json")).unwrap(),
            before
        );
        assert!(
            controller
                .project(&fixture.keys.secret_key, ProjectAction::Keys)
                .is_ok()
        );
    }

    #[test]
    fn key_changes_preserve_in_flight_admissions_and_access_state() {
        let mut fixture = Fixture::new();
        let access = fixture
            .registry
            .authorize(&fixture.keys.public_key, projects::Permission::Public)
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
    fn deletion_revokes_default_keys_and_preserves_data_until_project_expiration() {
        let mut fixture = Fixture::new();
        Arc::get_mut(&mut fixture.config).unwrap().retention_seconds = 0;
        execute(
            &fixture.config,
            &mut fixture.keys,
            Some(&fixture.registry),
            &request("project-retention", &["default", "10"]),
        )
        .unwrap();
        let mut db = duckdb::Connection::open(fixture.config.database_path()).unwrap();
        db.execute_batch(
            "CREATE TABLE events(project_id VARCHAR, event_id VARCHAR, received_at_ms BIGINT);
            CREATE TABLE event_otlp(project_id VARCHAR, event_id VARCHAR);
            INSERT INTO events VALUES ('default', 'retained', 1000);
            INSERT INTO event_otlp VALUES ('default', 'retained');",
        )
        .unwrap();
        let controller = Controller::new(
            fixture.config.clone(),
            fixture.registry.clone(),
            fixture.keys.clone(),
        );
        let result = controller
            .project(&fixture.keys.secret_key, ProjectAction::Delete)
            .ok()
            .unwrap();
        assert_eq!(result["deleted"], true);
        assert_eq!(result["restart_required"], false);
        assert!(
            fixture
                .registry
                .authorize(&fixture.keys.public_key, projects::Permission::Public)
                .is_err()
        );
        assert!(
            fixture
                .registry
                .authorize(&fixture.keys.secret_key, projects::Permission::Secret)
                .is_err()
        );
        let saved = fixture.config.keys().unwrap();
        assert!(saved.projects.as_ref().unwrap()[0].deleted);
        assert!(saved.projects.as_ref().unwrap()[0].keys.is_empty());
        let restored = Registry::new(&saved, &fixture.config).unwrap();
        assert!(
            restored
                .authorize(&saved.secret_key, projects::Permission::Secret)
                .is_err()
        );
        let policy = restored.retention.read().unwrap();
        assert_eq!(policy.purge(&mut db, 11000).unwrap(), 0);
        assert_eq!(
            db.query_row("SELECT count(*) FROM events", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(policy.purge(&mut db, 11001).unwrap(), 1);
        assert_eq!(
            db.query_row("SELECT count(*) FROM event_otlp", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(
            controller
                .project(&fixture.keys.secret_key, ProjectAction::Delete)
                .is_err()
        );
    }

    #[test]
    fn failed_rename_keeps_disk_and_active_keys_unchanged() {
        let mut fixture = Fixture::new();
        let file = fixture.directory.join("keys.json");
        let backup = fixture.directory.join("original.json");
        let before = fs::read(&file).unwrap();
        fs::rename(&file, &backup).unwrap();
        fs::create_dir(&file).unwrap();
        assert!(
            execute(
                &fixture.config,
                &mut fixture.keys,
                Some(&fixture.registry),
                &request("key-rotate", &["default", "primary-public"])
            )
            .is_err()
        );
        assert!(
            fixture
                .registry
                .authorize(&fixture.keys.public_key, projects::Permission::Public)
                .is_ok()
        );
        assert_eq!(serde_json::to_vec_pretty(&fixture.keys).unwrap(), before);
        assert!(!fs::read_dir(&fixture.directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".keys-")
        }));
        fs::remove_dir(file).unwrap();
        fs::rename(backup, fixture.directory.join("keys.json")).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_socket_rejects_invalid_requests_and_cleans_up() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::UnixStream,
        };
        let fixture = Fixture::new();
        let _lock = fixture.config.lock().unwrap();
        let path = fixture.directory.join("admin.sock");
        // Never delete a regular file found where the socket should be.
        fs::write(&path, b"keep").unwrap();
        assert!(
            Server::bind(
                fixture.config.clone(),
                fixture.registry.clone(),
                fixture.keys.clone()
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        fs::remove_file(&path).unwrap();
        let server = Server::bind(
            fixture.config.clone(),
            fixture.registry.clone(),
            fixture.keys.clone(),
        )
        .unwrap()
        .start();
        for body in [
            "invalid".into(),
            "x".repeat(16385),
            serde_json::to_string(&request("project-rename", &["default", "blocked"])).unwrap(),
            r#"{"command":"project-list","args":[],"unexpected":true}"#.into(),
        ] {
            let mut stream = UnixStream::connect(&path).await.unwrap();
            stream.write_all(body.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            assert!(serde_json::from_slice::<Reply>(&bytes).unwrap().is_err());
        }
        assert_eq!(
            live::send(&fixture.config, &request("project-list", &[]))
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        server.stop().await;
        assert!(!path.exists());
    }
}
