//! Storage selection and connection settings. Credentials are never formatted in diagnostics.
use crate::settings::Settings;
use std::path::PathBuf;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone)]
pub enum Backend {
    Local { path: Option<PathBuf> },
    S3(S3Config),
}

#[derive(Clone)]
pub struct S3Config {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub url_style: String,
    pub use_ssl: bool,
}

impl Default for Backend {
    fn default() -> Self {
        Self::Local { path: None }
    }
}

impl Backend {
    pub fn load(settings: &Settings) -> Result<Self> {
        match settings
            .var("COLLECTOR_STORAGE_BACKEND")
            .as_deref()
            .unwrap_or("local")
        {
            "local" => Ok(Self::Local {
                path: settings.var_os("COLLECTOR_LOCAL_PATH").map(PathBuf::from),
            }),
            "s3" => Ok(Self::S3(S3Config::load(settings)?)),
            _ => Err("COLLECTOR_STORAGE_BACKEND must be local or s3".into()),
        }
    }
}

impl S3Config {
    fn load(settings: &Settings) -> Result<Self> {
        let optional = |name| settings.var(name).ok().filter(|value| !value.is_empty());
        let url = optional("COLLECTOR_S3_URL").ok_or("S3 storage requires COLLECTOR_S3_URL")?;
        let parsed = reqwest::Url::parse(&url).map_err(|_| "Invalid S3 storage URL")?;
        let bucket = parsed
            .host_str()
            .ok_or("S3 storage URL requires a bucket")?;
        let prefix = parsed.path().trim_matches('/');
        if parsed.scheme() != "s3"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !bucket
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
            || prefix.split('/').any(|part| {
                part == ".."
                    || part == "."
                    || part.contains(['*', '?', '[', ']', '%', '\\'])
                    || part.chars().any(char::is_control)
            })
        {
            return Err("S3 storage URL must be s3://bucket/optional-prefix without credentials, escapes or wildcards".into());
        }
        let access_key_id = optional("COLLECTOR_S3_ACCESS_KEY_ID");
        let secret_access_key = optional("COLLECTOR_S3_SECRET_ACCESS_KEY");
        if access_key_id.is_some() != secret_access_key.is_some() {
            return Err("S3 access_key_id and secret_access_key must be provided together".into());
        }
        let region = optional("COLLECTOR_S3_REGION").unwrap_or_else(|| "us-east-1".into());
        let endpoint = optional("COLLECTOR_S3_ENDPOINT");
        if let Some(endpoint) = &endpoint
            && (endpoint.contains(['/', '@', '?', '#'])
                || endpoint.parse::<axum::http::uri::Authority>().is_err())
        {
            return Err(
                "S3 endpoint must be a hostname with an optional port; use use_ssl to select HTTPS"
                    .into(),
            );
        }
        let url_style = optional("COLLECTOR_S3_URL_STYLE").unwrap_or_else(|| "path".into());
        if !matches!(url_style.as_str(), "path" | "vhost") {
            return Err("S3 url_style must be path or vhost".into());
        }
        let use_ssl = settings
            .var("COLLECTOR_S3_USE_SSL")
            .unwrap_or_else(|_| "true".into())
            .parse()
            .map_err(|_| "S3 use_ssl must be true or false")?;
        Ok(Self {
            bucket: bucket.into(),
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            },
            endpoint,
            region,
            access_key_id,
            secret_access_key,
            session_token: optional("COLLECTOR_S3_SESSION_TOKEN"),
            url_style,
            use_ssl,
        })
    }
}
