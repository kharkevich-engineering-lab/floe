//! The Iceberg REST [`Committer`] (`docs/design/github-mirror.md` §C.5,
//! feature `iceberg`): the only code that talks to the catalog.
//!
//! `connect` builds the REST client (bearer token or `OAuth2` client
//! credentials from env vars named in `[catalog]`, never from the config
//! itself), creates the namespace and the four tables when allowed, and checks
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
use iceberg_storage_opendal::OpenDalStorageFactory;

use crate::buffer::{CommitError, Committer};
use crate::rows::{Row, Table, Timestamp};
use crate::schema;
use floe_config::CatalogConfig;

/// Conflict retries `Transaction::commit` does on the tables floe creates.
const COMMIT_RETRIES: &str = "5";

/// The live catalog client; replaced on every (re)connect.
struct Connected {
    catalog: Arc<RestCatalog>,
    namespace: NamespaceIdent,
}

pub struct IcebergCommitter {
    cfg: CatalogConfig,
    conn: tokio::sync::Mutex<Option<Arc<Connected>>>,
}

impl IcebergCommitter {
    pub fn new(cfg: &CatalogConfig) -> IcebergCommitter {
        IcebergCommitter {
            cfg: cfg.clone(),
            conn: tokio::sync::Mutex::new(None),
        }
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
        if let Some(var) = &cfg.token_env {
            props.insert("token".into(), required_env(var)?);
        }
        if let Some(var) = &cfg.credential_env {
            props.insert("credential".into(), required_env(var)?);
        }
        if let Some(endpoint) = &cfg.s3_endpoint {
            props.insert(S3_ENDPOINT.into(), endpoint.clone());
        }
        props.insert(S3_REGION.into(), cfg.s3_region.clone());
        props.insert(S3_PATH_STYLE_ACCESS.into(), cfg.s3_path_style.to_string());
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
        let catalog = RestCatalogBuilder::default()
            .with_storage_factory(Arc::new(OpenDalStorageFactory::S3 {
                customized_credential_load: None,
            }))
            .load("floe", self.props()?)
            .await?;
        let conn = Connected {
            catalog: Arc::new(catalog),
            namespace: NamespaceIdent::new(self.cfg.namespace.clone()),
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
