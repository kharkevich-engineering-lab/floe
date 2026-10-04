//! The Iceberg REST [`Committer`] (`docs/design/github-mirror.md` §C.5,
//! feature `iceberg`): the only code that talks to the catalog.
//!
//! `connect` builds the REST client (`[catalog] auth`: none, a bearer token or
//! `OAuth2` client credentials from env vars named in `[catalog]`, never from
//! the config itself, or `SigV4` through [`crate::sigv4`], D63), creates the
//! namespace and the four tables when allowed, and checks
//! that each table's schema is floe's. `commit` writes one Parquet data file
//! per partition through the table's `FileIO` and fast-appends them in one
//! snapshot. `Transaction::commit` reloads the table and retries a conflict
//! (another instance appended) up to `commit.retry.num-retries` (set to 5 on
//! the tables floe creates).

use std::collections::HashMap;
use std::sync::Arc;

use ::iceberg::arrow::RecordBatchPartitionSplitter;
use ::iceberg::io::{
    S3_ACCESS_KEY_ID, S3_ENDPOINT, S3_PATH_STYLE_ACCESS, S3_REGION, S3_SECRET_ACCESS_KEY,
};
use ::iceberg::spec::DataFileFormat;
use ::iceberg::table::Table as IcebergTable;
use ::iceberg::transaction::{ApplyTransactionAction, Transaction};
use ::iceberg::writer::base_writer::data_file_writer::DataFileWriterBuilder;
use ::iceberg::writer::file_writer::ParquetWriterBuilder;
use ::iceberg::writer::file_writer::location_generator::{
    DefaultFileNameGenerator, DefaultLocationGenerator,
};
use ::iceberg::writer::file_writer::rolling_writer::RollingFileWriterBuilder;
use ::iceberg::writer::partitioning::PartitioningWriter;
use ::iceberg::writer::partitioning::fanout_writer::FanoutWriter;
use ::iceberg::writer::{IcebergWriter, IcebergWriterBuilder};
use ::iceberg::{
    Catalog, CatalogBuilder, Error, ErrorKind, NamespaceIdent, TableCreation, TableIdent,
};
use iceberg_catalog_rest::{
    REST_CATALOG_PROP_URI, REST_CATALOG_PROP_WAREHOUSE, RestCatalog, RestCatalogBuilder,
};
use iceberg_storage_opendal::{
    AwsCredential, CustomAwsCredentialLoader, OpenDalStorageFactory, ProvideCredential,
};

use crate::buffer::{CommitError, Committer};
use crate::rows::{Row, Table, Timestamp};
use crate::schema;
use crate::sigv4::{CredentialSource, Signer, SigningProxy};
use floe_config::{CatalogAuth, CatalogConfig};

/// Conflict retries `Transaction::commit` does on the tables floe creates.
const COMMIT_RETRIES: &str = "5";

/// The live catalog client; replaced on every (re)connect.
struct Connected {
    catalog: Arc<RestCatalog>,
    namespace: NamespaceIdent,
    /// `auth = "sigv4"`: the signing hop the catalog's client talks to; it
    /// lives exactly as long as this connection.
    _proxy: Option<SigningProxy>,
}

pub struct IcebergCommitter {
    cfg: CatalogConfig,
    conn: tokio::sync::Mutex<Option<Arc<Connected>>>,
    /// `auth = "sigv4"`: loaded on the first connect and kept across
    /// reconnects, so the AWS chain's cache survives a catalog outage.
    creds: tokio::sync::OnceCell<Arc<CredentialSource>>,
}

impl IcebergCommitter {
    pub fn new(cfg: &CatalogConfig) -> IcebergCommitter {
        IcebergCommitter {
            cfg: cfg.clone(),
            conn: tokio::sync::Mutex::new(None),
            creds: tokio::sync::OnceCell::new(),
        }
    }

    /// The D43 credentials that sign catalog requests and write data files.
    async fn credentials(&self) -> Result<Arc<CredentialSource>, Error> {
        let cfg = &self.cfg;
        self.creds
            .get_or_try_init(|| async {
                CredentialSource::from_env(
                    &cfg.s3_access_key_env,
                    &cfg.s3_secret_key_env,
                    cfg.sigv4_region(),
                )
                .await
                .map(Arc::new)
                .map_err(|e| Error::new(ErrorKind::DataInvalid, format!("catalog: {e}")))
            })
            .await
            .cloned()
    }

    /// The REST catalog for this config. With `SigV4`, every request goes
    /// through a fresh [`SigningProxy`], and the data-file `FileIO` uses the
    /// same credentials (the endpoint and path style from `s3_*`).
    async fn build_catalog(&self) -> Result<(RestCatalog, Option<SigningProxy>), Error> {
        let mut props = self.props()?;
        let mut builder = RestCatalogBuilder::default();
        let mut loader = None;
        let mut proxy = None;
        if self.cfg.auth == CatalogAuth::Sigv4 {
            let creds = self.credentials().await?;
            let signer = Signer::new(
                creds.clone(),
                &self.cfg.sigv4_service,
                self.cfg.sigv4_region(),
            );
            let uri = props
                .get(REST_CATALOG_PROP_URI)
                .cloned()
                .unwrap_or_default();
            let p = SigningProxy::start(&uri, signer)
                .map_err(|e| Error::new(ErrorKind::DataInvalid, format!("catalog: {e}")))?;
            props.insert(
                REST_CATALOG_PROP_URI.to_string(),
                p.catalog_uri().to_string(),
            );
            builder = builder.with_client(p.client());
            loader = Some(CustomAwsCredentialLoader::new(FileIoCredentials(creds)));
            proxy = Some(p);
        }
        let catalog = builder
            .with_storage_factory(Arc::new(OpenDalStorageFactory::S3 {
                customized_credential_load: loader,
            }))
            .load("floe", props)
            .await?;
        Ok((catalog, proxy))
    }

    /// The current state of one of floe's tables (operators, tests).
    pub async fn load_table(&self, table: Table) -> Result<IcebergTable, Error> {
        let c = self.connected().await?;
        let ident = TableIdent::new(c.namespace.clone(), table.name().to_string());
        c.catalog.load_table(&ident).await
    }

    async fn connected(&self) -> Result<Arc<Connected>, Error> {
        self.conn
            .lock()
            .await
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::Unexpected, "catalog not connected"))
    }

    async fn ensure_tables(&self, c: &Connected) -> Result<(), Error> {
        let catalog = c.catalog.as_ref();
        if !catalog.namespace_exists(&c.namespace).await? {
            if !self.cfg.create_tables {
                return Err(missing(&format!("namespace {}", self.cfg.namespace)));
            }
            match catalog.create_namespace(&c.namespace, HashMap::new()).await {
                Ok(_) => {
                    tracing::info!(namespace = %self.cfg.namespace, "catalog: namespace created");
                }
                // Another instance won the race.
                Err(e) if e.kind() == ErrorKind::NamespaceAlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        for table in Table::ALL {
            let ident = TableIdent::new(c.namespace.clone(), table.name().to_string());
            if !catalog.table_exists(&ident).await? {
                if !self.cfg.create_tables {
                    return Err(missing(&format!("table {}", table.name())));
                }
                let creation = TableCreation::builder()
                    .name(table.name().to_string())
                    .schema(schema::schema(table)?)
                    .partition_spec(schema::partition_spec(table)?)
                    .sort_order(schema::sort_order(table)?)
                    .properties([(
                        ::iceberg::spec::TableProperties::PROPERTY_COMMIT_NUM_RETRIES.to_string(),
                        COMMIT_RETRIES.to_string(),
                    )])
                    .build();
                match catalog.create_table(&c.namespace, creation).await {
                    Ok(_) => tracing::info!(%table, "catalog: table created"),
                    Err(e) if e.kind() == ErrorKind::TableAlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            let loaded = catalog.load_table(&ident).await?;
            schema::check_compatible(table, loaded.metadata().current_schema())?;
        }
        Ok(())
    }

    /// REST + `FileIO` properties. Secrets come from env vars only.
    fn props(&self) -> Result<HashMap<String, String>, Error> {
        let cfg = &self.cfg;
        let mut props = HashMap::new();
        let uri = cfg
            .uri
            .clone()
            .ok_or_else(|| Error::new(ErrorKind::DataInvalid, "catalog.uri is not set"))?;
        props.insert(REST_CATALOG_PROP_URI.to_string(), uri);
        if let Some(w) = &cfg.warehouse {
            props.insert(REST_CATALOG_PROP_WAREHOUSE.to_string(), w.clone());
        }
        if cfg.auth == CatalogAuth::Bearer {
            if let Some(var) = &cfg.token_env {
                props.insert("token".into(), required_env(var)?);
            }
            if let Some(var) = &cfg.credential_env {
                props.insert("credential".into(), required_env(var)?);
            }
        }
        if let Some(endpoint) = &cfg.s3_endpoint {
            props.insert(S3_ENDPOINT.into(), endpoint.clone());
        }
        props.insert(S3_REGION.into(), cfg.s3_region.clone());
        props.insert(S3_PATH_STYLE_ACCESS.into(), cfg.s3_path_style.to_string());
        if cfg.auth == CatalogAuth::Sigv4 {
            // FileIO gets the signing credentials through a loader instead.
            return Ok(props);
        }
        // Optional: a catalog that vends credentials needs neither.
        if let Ok(key) = std::env::var(&cfg.s3_access_key_env) {
            props.insert(S3_ACCESS_KEY_ID.into(), key);
        }
        if let Ok(secret) = std::env::var(&cfg.s3_secret_key_env) {
            props.insert(S3_SECRET_ACCESS_KEY.into(), secret);
        }
        Ok(props)
    }
}

#[async_trait::async_trait]
impl Committer for IcebergCommitter {
    async fn connect(&self) -> Result<(), CommitError> {
        let (catalog, proxy) = self.build_catalog().await?;
        let conn = Connected {
            catalog: Arc::new(catalog),
            namespace: NamespaceIdent::new(self.cfg.namespace.clone()),
            _proxy: proxy,
        };
        self.ensure_tables(&conn).await?;
        *self.conn.lock().await = Some(Arc::new(conn));
        Ok(())
    }

    async fn commit(
        &self,
        table: Table,
        rows: &[Row],
        ingested_at: Timestamp,
    ) -> Result<(), CommitError> {
        let c = self.connected().await?;
        let ident = TableIdent::new(c.namespace.clone(), table.name().to_string());
        let loaded = c.catalog.load_table(&ident).await?;
        let files = write_data_files(table, &loaded, rows, ingested_at).await?;
        if files.is_empty() {
            return Ok(());
        }
        let tx = Transaction::new(&loaded);
        let tx = tx.fast_append().add_data_files(files).apply(tx)?;
        tx.commit(c.catalog.as_ref()).await?;
        Ok(())
    }
}

/// One Parquet file per partition touched (all rows of one flush).
async fn write_data_files(
    table: Table,
    loaded: &IcebergTable,
    rows: &[Row],
    ingested_at: Timestamp,
) -> Result<Vec<::iceberg::spec::DataFile>, Error> {
    let metadata = loaded.metadata();
    let schema = metadata.current_schema().clone();
    let batch = schema::record_batch(table, &schema, rows, ingested_at)?;
    let builder = DataFileWriterBuilder::new(RollingFileWriterBuilder::new_with_default_file_size(
        ParquetWriterBuilder::from_table_properties(&metadata.table_properties()?, schema.clone()),
        loaded.file_io().clone(),
        DefaultLocationGenerator::new(metadata)?,
        DefaultFileNameGenerator::new(
            format!("floe-{}", uuid::Uuid::new_v4()),
            None,
            DataFileFormat::Parquet,
        ),
    ));
    let spec = metadata.default_partition_spec().clone();
    if spec.is_unpartitioned() {
        let mut writer = builder.build(None).await?;
        writer.write(batch).await?;
        return writer.close().await;
    }
    let splitter = RecordBatchPartitionSplitter::try_new_with_computed_values(schema, spec)?;
    let mut writer = FanoutWriter::new(builder);
    for (key, part) in splitter.split(&batch)? {
        writer.write(key, part).await?;
    }
    writer.close().await
}

/// The data-file `FileIO`'s credentials under `auth = "sigv4"`: the same
/// [`CredentialSource`] that signs the catalog requests, with its expiry, so
/// opendal reloads temporary credentials when they run out.
#[derive(Debug)]
struct FileIoCredentials(Arc<CredentialSource>);

impl ProvideCredential for FileIoCredentials {
    type Credential = AwsCredential;

    async fn provide_credential(
        &self,
        _ctx: &reqsign_core::Context,
    ) -> reqsign_core::Result<Option<AwsCredential>> {
        let c = self
            .0
            .get()
            .await
            .map_err(|e| reqsign_core::Error::credential_invalid(e.to_string()))?;
        let expires_in = c
            .expiry()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .and_then(|secs| reqsign_core::time::Timestamp::from_second(secs).ok());
        Ok(Some(AwsCredential {
            access_key_id: c.access_key_id().to_string(),
            secret_access_key: c.secret_access_key().to_string(),
            session_token: c.session_token().map(str::to_string),
            expires_in,
        }))
    }
}

fn required_env(var: &str) -> Result<String, Error> {
    std::env::var(var).map_err(|_| {
        Error::new(
            ErrorKind::DataInvalid,
            format!("catalog: env var {var} (named in [catalog]) is not set"),
        )
    })
}

fn missing(what: &str) -> Error {
    Error::new(
        ErrorKind::DataInvalid,
        format!("catalog: {what} is missing and catalog.create_tables = false"),
    )
}

#[cfg(test)]
// Helpers outside #[test] fns fail the test the same way.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    use aws_credential_types::Credentials;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use axum::response::IntoResponse;

    use super::*;
    use crate::buffer::Committer;
    use crate::sigv4::{SigningInput, sign_headers};

    #[derive(Debug, Clone)]
    struct Seen {
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    /// A catalog that records every request: `/v1/config`, no namespace (created),
    /// no tables, and `create_table` fails, which ends `connect`.
    async fn fake_catalog(
        State(log): State<Log>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        log.lock().unwrap().push(Seen {
            method: method.clone(),
            uri: uri.clone(),
            headers,
            body: body.to_vec(),
        });
        let path = uri.path();
        let json =
            |status: StatusCode, v: serde_json::Value| (status, axum::Json(v)).into_response();
        match (method, path) {
            (Method::GET, "/iceberg/v1/config") => json(
                StatusCode::OK,
                serde_json::json!({"defaults": {"prefix": "floe-catalog"}, "overrides": {}}),
            ),
            (Method::HEAD, _) => StatusCode::NOT_FOUND.into_response(),
            (Method::POST, "/iceberg/v1/floe-catalog/namespaces") => json(
                StatusCode::OK,
                serde_json::json!({"namespace": ["floe"], "properties": {}}),
            ),
            _ => json(
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({"error": {"message": "fake", "type": "ServiceUnavailableException", "code": 503}}),
            ),
        }
    }

    fn header<'a>(h: &'a HeaderMap, name: &str) -> &'a str {
        h.get(name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .to_str()
            .unwrap()
    }

    /// Every catalog request `connect` makes (config, namespace HEAD + create,
    /// table HEAD + create) reaches the endpoint signed (`SigV4`), with the session
    /// token, and the signature verifies against the request as received.
    #[tokio::test]
    async fn every_catalog_request_is_sigv4_signed() {
        let log: Log = Arc::default();
        let app = axum::Router::new()
            .fallback(fake_catalog)
            .with_state(log.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = CatalogConfig {
            enabled: true,
            uri: Some(format!("http://{addr}/iceberg")),
            warehouse: Some("floe-catalog".into()),
            auth: CatalogAuth::Sigv4,
            s3_endpoint: Some(format!("http://{addr}")),
            ..CatalogConfig::default()
        };
        let creds = Credentials::new(
            "AKIDTEST",
            "secret-test",
            Some("tok-test".into()),
            None,
            "t",
        );
        let committer = IcebergCommitter::new(&cfg);
        committer
            .creds
            .set(Arc::new(CredentialSource::fixed(creds.clone())))
            .unwrap();
        let err = committer.connect().await.err().expect("create_table fails");
        assert!(format!("{err:?}").contains("fake"), "{err:?}");

        let seen = log.lock().unwrap().clone();
        let methods: Vec<_> = seen
            .iter()
            .map(|s| (s.method.clone(), s.uri.path().to_string()))
            .collect();
        assert!(seen.len() >= 5, "{methods:?}");
        for m in [Method::GET, Method::HEAD, Method::POST] {
            assert!(seen.iter().any(|s| s.method == m), "no {m} in {methods:?}");
        }
        for s in &seen {
            let auth = header(&s.headers, "authorization");
            let date = header(&s.headers, "x-amz-date");
            assert!(
                auth.starts_with(&format!(
                    "AWS4-HMAC-SHA256 Credential=AKIDTEST/{}/us-east-1/s3/aws4_request, SignedHeaders=",
                    date.get(..8).unwrap()
                )),
                "{} {}: {auth}",
                s.method,
                s.uri
            );
            assert_eq!(header(&s.headers, "x-amz-security-token"), "tok-test");
            assert_eq!(header(&s.headers, "host"), addr.to_string());
            let signed: Vec<&str> = auth
                .split("SignedHeaders=")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .split(';')
                .collect();
            for h in [
                "host",
                "x-amz-content-sha256",
                "x-amz-date",
                "x-amz-security-token",
            ] {
                assert!(signed.contains(&h), "{h} not signed: {auth}");
            }

            // Verify: re-sign what arrived (the signed headers the signer did
            // not add itself) at the request's own X-Amz-Date.
            let time = chrono::NaiveDateTime::parse_from_str(date, "%Y%m%dT%H%M%SZ")
                .unwrap()
                .and_utc()
                .timestamp();
            let mut again = HeaderMap::new();
            for name in &signed {
                if !name.starts_with("x-amz-") {
                    for v in s.headers.get_all(*name) {
                        again.append(
                            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                            v.clone(),
                        );
                    }
                }
            }
            let url = format!("http://{addr}{}", s.uri);
            let input = SigningInput {
                method: s.method.as_str(),
                url: &url,
                body: &s.body,
                service: "s3",
                region: "us-east-1",
                time: UNIX_EPOCH + Duration::from_secs(u64::try_from(time).unwrap()),
                payload_header: true,
            };
            sign_headers(&input, &mut again, &creds).unwrap();
            assert_eq!(
                header(&again, "authorization"),
                auth,
                "{} {}",
                s.method,
                s.uri
            );
        }
    }

    /// The proxy signs only for the configured host: a `/v1/config` that
    /// moves `uri` elsewhere is refused, not signed.
    #[tokio::test]
    async fn signing_proxy_refuses_other_hosts() {
        let signer = Signer::new(
            Arc::new(CredentialSource::fixed(Credentials::new(
                "a", "b", None, None, "t",
            ))),
            "s3",
            "us-east-1",
        );
        let proxy = SigningProxy::start("https://catalog.example.com/iceberg/", signer).unwrap();
        assert_eq!(proxy.catalog_uri(), "http://catalog.example.com/iceberg");
        let resp = proxy
            .client()
            .get("http://elsewhere.example.com/iceberg/v1/config")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::MISDIRECTED_REQUEST);
        let body = resp.text().await.unwrap();
        assert!(body.contains("elsewhere.example.com"), "{body}");
        assert!(
            SigningProxy::start(
                "http://u:p@h/iceberg",
                Signer::new(
                    Arc::new(CredentialSource::fixed(Credentials::new(
                        "a", "b", None, None, "t"
                    ))),
                    "s3",
                    "us-east-1",
                )
            )
            .is_err()
        );
    }
}
