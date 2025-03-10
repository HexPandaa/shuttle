mod deployment;
mod error;
mod log;
mod resource;
mod secret;
mod service;
mod state;
mod user;

use crate::deployment::deploy_layer::{self, LogRecorder, LogType};
use crate::deployment::ActiveDeploymentsGetter;
use crate::proxy::AddressGetter;
use error::{Error, Result};

use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;

use chrono::Utc;
use serde_json::json;
use shuttle_common::STATE_MESSAGE;
use sqlx::migrate::{MigrateDatabase, Migrator};
use sqlx::sqlite::{Sqlite, SqliteConnectOptions, SqliteJournalMode, SqlitePool};
use tokio::sync::broadcast::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tracing::{error, info, instrument, trace};
use uuid::Uuid;

use self::deployment::DeploymentRunnable;
pub use self::deployment::{Deployment, DeploymentState};
pub use self::error::Error as PersistenceError;
pub use self::log::{Level as LogLevel, Log};
pub use self::resource::{Resource, ResourceManager, Type as ResourceType};
use self::secret::Secret;
pub use self::secret::{SecretGetter, SecretRecorder};
pub use self::service::Service;
pub use self::state::State;
pub use self::user::User;

pub static MIGRATIONS: Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct Persistence {
    pool: SqlitePool,
    log_send: crossbeam_channel::Sender<deploy_layer::Log>,
    stream_log_send: Sender<deploy_layer::Log>,
}

impl Persistence {
    /// Creates a persistent storage solution (i.e., SQL database). This
    /// function creates all necessary tables and sets up a database connection
    /// pool - new connections should be made by cloning [`Persistence`] rather
    /// than repeatedly calling [`Persistence::new`].
    pub async fn new(path: &str) -> (Self, JoinHandle<()>) {
        if !Path::new(path).exists() {
            Sqlite::create_database(path).await.unwrap();
        }

        info!(
            "state db: {}",
            std::fs::canonicalize(path).unwrap().to_string_lossy()
        );

        // We have found in the past that setting synchronous to anything other than the default (full) breaks the
        // broadcast channel in deployer. The broken symptoms are that the ws socket connections won't get any logs
        // from the broadcast channel and would then close. When users did deploys, this would make it seem like the
        // deploy is done (while it is still building for most of the time) and the status of the previous deployment
        // would be returned to the user.
        //
        // If you want to activate a faster synchronous mode, then also do proper testing to confirm this bug is no
        // longer present.
        let sqlite_options = SqliteConnectOptions::from_str(path)
            .unwrap()
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePool::connect_with(sqlite_options).await.unwrap();

        Self::from_pool(pool).await
    }

    #[allow(dead_code)]
    async fn new_in_memory() -> (Self, JoinHandle<()>) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> (Self, JoinHandle<()>) {
        MIGRATIONS.run(&pool).await.unwrap();

        let (log_send, log_recv): (crossbeam_channel::Sender<deploy_layer::Log>, _) =
            crossbeam_channel::bounded(0);

        let (stream_log_send, _) = broadcast::channel(1);
        let stream_log_send_clone = stream_log_send.clone();

        let pool_cloned = pool.clone();

        // The logs are received on a non-async thread.
        // This moves them to an async thread
        let handle = tokio::spawn(async move {
            while let Ok(log) = log_recv.recv() {
                trace!(?log, "persistence received got log");
                match log.r#type {
                    LogType::Event => {
                        insert_log(&pool_cloned, log.clone())
                            .await
                            .unwrap_or_else(|error| {
                                error!(
                                    error = &error as &dyn std::error::Error,
                                    "failed to insert event log"
                                )
                            });
                    }
                    LogType::State => {
                        insert_log(
                            &pool_cloned,
                            Log {
                                id: log.id,
                                timestamp: log.timestamp,
                                state: log.state,
                                level: log.level.clone(),
                                file: log.file.clone(),
                                line: log.line,
                                target: String::new(),
                                fields: json!(STATE_MESSAGE),
                            },
                        )
                        .await
                        .unwrap_or_else(|error| {
                            error!(
                                error = &error as &dyn std::error::Error,
                                "failed to insert state log"
                            )
                        });
                        update_deployment(&pool_cloned, log.clone())
                            .await
                            .unwrap_or_else(|error| {
                                error!(
                                    error = &error as &dyn std::error::Error,
                                    "failed to update deployment state"
                                )
                            });
                    }
                };

                let receiver_count = stream_log_send_clone.receiver_count();
                trace!(?log, receiver_count, "sending log to broadcast stream");

                if receiver_count > 0 {
                    stream_log_send_clone.send(log).unwrap_or_else(|error| {
                        error!(
                            error = &error as &dyn std::error::Error,
                            "failed to broadcast log"
                        );

                        0
                    });
                }
            }
        });

        let persistence = Self {
            pool,
            log_send,
            stream_log_send,
        };

        (persistence, handle)
    }

    pub async fn insert_deployment(&self, deployment: impl Into<Deployment>) -> Result<()> {
        let deployment = deployment.into();

        sqlx::query(
            "INSERT INTO deployments (id, service_id, state, last_update, address) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(deployment.id)
        .bind(deployment.service_id)
        .bind(deployment.state)
        .bind(deployment.last_update)
        .bind(deployment.address.map(|socket| socket.to_string()))
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(Error::from)
    }

    pub async fn get_deployment(&self, id: &Uuid) -> Result<Option<Deployment>> {
        get_deployment(&self.pool, id).await
    }

    pub async fn get_deployments(&self, service_id: &Uuid) -> Result<Vec<Deployment>> {
        sqlx::query_as("SELECT * FROM deployments WHERE service_id = ?")
            .bind(service_id)
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)
    }

    pub async fn get_active_deployment(&self, service_id: &Uuid) -> Result<Option<Deployment>> {
        sqlx::query_as("SELECT * FROM deployments WHERE service_id = ? AND state = ?")
            .bind(service_id)
            .bind(State::Running)
            .fetch_optional(&self.pool)
            .await
            .map_err(Error::from)
    }

    // Clean up all invalid states inside persistence
    pub async fn cleanup_invalid_states(&self) -> Result<()> {
        sqlx::query("UPDATE deployments SET state = ? WHERE state IN(?, ?, ?, ?)")
            .bind(State::Stopped)
            .bind(State::Queued)
            .bind(State::Built)
            .bind(State::Building)
            .bind(State::Loading)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn get_or_create_service(&self, name: &str) -> Result<Service> {
        if let Some(service) = self.get_service_by_name(name).await? {
            Ok(service)
        } else {
            let service = Service {
                id: Uuid::new_v4(),
                name: name.to_string(),
            };

            sqlx::query("INSERT INTO services (id, name) VALUES (?, ?)")
                .bind(service.id)
                .bind(&service.name)
                .execute(&self.pool)
                .await?;

            Ok(service)
        }
    }

    pub async fn get_service_by_name(&self, name: &str) -> Result<Option<Service>> {
        sqlx::query_as("SELECT * FROM services WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(Error::from)
    }

    pub async fn delete_service(&self, id: &Uuid) -> Result<()> {
        sqlx::query("DELETE FROM services WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(Error::from)
    }

    pub async fn get_all_services(&self) -> Result<Vec<Service>> {
        sqlx::query_as("SELECT * FROM services")
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)
    }

    pub async fn get_all_runnable_deployments(&self) -> Result<Vec<DeploymentRunnable>> {
        sqlx::query_as(
            r#"SELECT d.id, service_id, s.name AS service_name
                FROM deployments AS d
                JOIN services AS s ON s.id = d.service_id
                WHERE state = ?
                ORDER BY last_update"#,
        )
        .bind(State::Running)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)
    }

    pub(crate) async fn get_deployment_logs(&self, id: &Uuid) -> Result<Vec<Log>> {
        // TODO: stress this a bit
        get_deployment_logs(&self.pool, id).await
    }

    pub fn get_log_subscriber(&self) -> Receiver<deploy_layer::Log> {
        self.stream_log_send.subscribe()
    }

    pub fn get_log_sender(&self) -> crossbeam_channel::Sender<deploy_layer::Log> {
        self.log_send.clone()
    }
}

async fn update_deployment(pool: &SqlitePool, state: impl Into<DeploymentState>) -> Result<()> {
    let state = state.into();

    // TODO: Handle moving to 'active_deployments' table for State::Running.

    sqlx::query("UPDATE deployments SET state = ?, last_update = ?, address = ? WHERE id = ?")
        .bind(state.state)
        .bind(state.last_update)
        .bind(state.address.map(|socket| socket.to_string()))
        .bind(state.id)
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(Error::from)
}

async fn get_deployment(pool: &SqlitePool, id: &Uuid) -> Result<Option<Deployment>> {
    sqlx::query_as("SELECT * FROM deployments WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Error::from)
}

async fn insert_log(pool: &SqlitePool, log: impl Into<Log>) -> Result<()> {
    let log = log.into();

    sqlx::query("INSERT INTO logs (id, timestamp, state, level, file, line, target, fields) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(log.id)
        .bind(log.timestamp)
        .bind(log.state)
        .bind(log.level)
        .bind(log.file)
        .bind(log.line)
        .bind(log.target)
        .bind(log.fields)
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(Error::from)
}

async fn get_deployment_logs(pool: &SqlitePool, id: &Uuid) -> Result<Vec<Log>> {
    sqlx::query_as("SELECT * FROM logs WHERE id = ? ORDER BY timestamp")
        .bind(id)
        .fetch_all(pool)
        .await
        .map_err(Error::from)
}

impl LogRecorder for Persistence {
    fn record(&self, log: deploy_layer::Log) {
        self.log_send
            .send(log)
            .expect("failed to move log to async thread");
    }
}

#[async_trait::async_trait]
impl ResourceManager for Persistence {
    type Err = Error;

    async fn insert_resource(&self, resource: &Resource) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO resources (service_id, type, data) VALUES (?, ?, ?)")
            .bind(resource.service_id)
            .bind(resource.r#type)
            .bind(&resource.data)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(Error::from)
    }

    async fn get_resources(&self, service_id: &Uuid) -> Result<Vec<Resource>> {
        sqlx::query_as(r#"SELECT * FROM resources WHERE service_id = ?"#)
            .bind(service_id)
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)
    }
}

#[async_trait::async_trait]
impl SecretRecorder for Persistence {
    type Err = Error;

    async fn insert_secret(&self, service_id: &Uuid, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO secrets (service_id, key, value, last_update) VALUES (?, ?, ?, ?)",
        )
        .bind(service_id)
        .bind(key)
        .bind(value)
        .bind(Utc::now())
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(Error::from)
    }
}

#[async_trait::async_trait]
impl SecretGetter for Persistence {
    type Err = Error;

    async fn get_secrets(&self, service_id: &Uuid) -> Result<Vec<Secret>> {
        sqlx::query_as("SELECT * FROM secrets WHERE service_id = ? ORDER BY key")
            .bind(service_id)
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)
    }
}

#[async_trait::async_trait]
impl AddressGetter for Persistence {
    #[instrument(skip(self))]
    async fn get_address_for_service(
        &self,
        service_name: &str,
    ) -> crate::handlers::Result<Option<std::net::SocketAddr>> {
        let address_str = sqlx::query_as::<_, (String,)>(
            r#"SELECT d.address
                FROM deployments AS d
                JOIN services AS s ON d.service_id = s.id
                WHERE s.name = ? AND d.state = ?
                ORDER BY d.last_update"#,
        )
        .bind(service_name)
        .bind(State::Running)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)
        .map_err(crate::handlers::Error::Persistence)?;

        if let Some((address_str,)) = address_str {
            SocketAddr::from_str(&address_str).map(Some).map_err(|err| {
                crate::handlers::Error::Convert {
                    from: "String".to_string(),
                    to: "SocketAddr".to_string(),
                    message: err.to_string(),
                }
            })
        } else {
            Ok(None)
        }
    }
}

#[async_trait::async_trait]
impl ActiveDeploymentsGetter for Persistence {
    type Err = Error;

    async fn get_active_deployments(
        &self,
        service_id: &Uuid,
    ) -> std::result::Result<Vec<Uuid>, Self::Err> {
        let ids: Vec<_> = sqlx::query_as::<_, Deployment>(
            "SELECT * FROM deployments WHERE service_id = ? AND state = ?",
        )
        .bind(service_id)
        .bind(State::Running)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?
        .into_iter()
        .map(|deployment| deployment.id)
        .collect();

        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use chrono::{TimeZone, Utc};
    use rand::Rng;
    use serde_json::json;

    use super::*;
    use crate::persistence::{
        deployment::{Deployment, DeploymentRunnable, DeploymentState},
        log::{Level, Log},
        state::State,
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn deployment_updates() {
        let (p, _) = Persistence::new_in_memory().await;
        let service_id = add_service(&p.pool).await.unwrap();

        let id = Uuid::new_v4();
        let deployment = Deployment {
            id,
            service_id,
            state: State::Queued,
            last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 43, 33).unwrap(),
            address: None,
        };

        p.insert_deployment(deployment.clone()).await.unwrap();
        assert_eq!(p.get_deployment(&id).await.unwrap().unwrap(), deployment);

        update_deployment(
            &p.pool,
            DeploymentState {
                id,
                state: State::Built,
                last_update: Utc::now(),
                address: None,
            },
        )
        .await
        .unwrap();
        let update = p.get_deployment(&id).await.unwrap().unwrap();
        assert_eq!(update.state, State::Built);
        assert_ne!(
            update.last_update,
            Utc.with_ymd_and_hms(2022, 4, 25, 4, 43, 33).unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deployment_active() {
        let (p, _) = Persistence::new_in_memory().await;

        let xyz_id = add_service(&p.pool).await.unwrap();
        let service_id = add_service(&p.pool).await.unwrap();

        let deployment_crashed = Deployment {
            id: Uuid::new_v4(),
            service_id: xyz_id,
            state: State::Crashed,
            last_update: Utc.with_ymd_and_hms(2022, 4, 25, 7, 29, 35).unwrap(),
            address: None,
        };
        let deployment_stopped = Deployment {
            id: Uuid::new_v4(),
            service_id: xyz_id,
            state: State::Stopped,
            last_update: Utc.with_ymd_and_hms(2022, 4, 25, 7, 49, 35).unwrap(),
            address: None,
        };
        let deployment_other = Deployment {
            id: Uuid::new_v4(),
            service_id,
            state: State::Running,
            last_update: Utc.with_ymd_and_hms(2022, 4, 25, 7, 39, 39).unwrap(),
            address: None,
        };
        let deployment_running = Deployment {
            id: Uuid::new_v4(),
            service_id: xyz_id,
            state: State::Running,
            last_update: Utc.with_ymd_and_hms(2022, 4, 25, 7, 48, 29).unwrap(),
            address: Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9876)),
        };

        for deployment in [
            &deployment_crashed,
            &deployment_stopped,
            &deployment_other,
            &deployment_running,
        ] {
            p.insert_deployment(deployment.clone()).await.unwrap();
        }

        assert_eq!(
            p.get_active_deployment(&xyz_id).await.unwrap().unwrap(),
            deployment_running
        );
    }

    // Test that we are correctly cleaning up any stale / unexpected states for a deployment
    // The reason this does not clean up two (or more) running states for a single deployment is because
    // it should theoretically be impossible for a service to have two deployments in the running state.
    // And even if a service where to have this, then the start ups of these deployments (more specifically
    // the last deployment that is starting up) will stop all the deployments correctly.
    #[tokio::test(flavor = "multi_thread")]
    async fn cleanup_invalid_states() {
        let (p, _) = Persistence::new_in_memory().await;

        let service_id = add_service(&p.pool).await.unwrap();

        let queued_id = Uuid::new_v4();
        let building_id = Uuid::new_v4();
        let built_id = Uuid::new_v4();
        let loading_id = Uuid::new_v4();

        let deployment_crashed = Deployment {
            id: Uuid::new_v4(),
            service_id,
            state: State::Crashed,
            last_update: Utc::now(),
            address: None,
        };
        let deployment_stopped = Deployment {
            id: Uuid::new_v4(),
            service_id,
            state: State::Stopped,
            last_update: Utc::now(),
            address: None,
        };
        let deployment_running = Deployment {
            id: Uuid::new_v4(),
            service_id,
            state: State::Running,
            last_update: Utc::now(),
            address: Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9876)),
        };
        let deployment_queued = Deployment {
            id: queued_id,
            service_id,
            state: State::Queued,
            last_update: Utc::now(),
            address: None,
        };
        let deployment_building = Deployment {
            id: building_id,
            service_id,
            state: State::Building,
            last_update: Utc::now(),
            address: None,
        };
        let deployment_built = Deployment {
            id: built_id,
            service_id,
            state: State::Built,
            last_update: Utc::now(),
            address: None,
        };
        let deployment_loading = Deployment {
            id: loading_id,
            service_id,
            state: State::Loading,
            last_update: Utc::now(),
            address: None,
        };

        for deployment in [
            &deployment_crashed,
            &deployment_stopped,
            &deployment_running,
            &deployment_queued,
            &deployment_built,
            &deployment_building,
            &deployment_loading,
        ] {
            p.insert_deployment(deployment.clone()).await.unwrap();
        }

        p.cleanup_invalid_states().await.unwrap();

        let actual: Vec<_> = p
            .get_deployments(&service_id)
            .await
            .unwrap()
            .into_iter()
            .map(|deployment| (deployment.id, deployment.state))
            .collect();
        let expected = vec![
            (deployment_crashed.id, State::Crashed),
            (deployment_stopped.id, State::Stopped),
            (deployment_running.id, State::Running),
            (queued_id, State::Stopped),
            (built_id, State::Stopped),
            (building_id, State::Stopped),
            (loading_id, State::Stopped),
        ];

        assert_eq!(
            actual, expected,
            "invalid states should be moved to the stopped state"
        );
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn fetching_runnable_deployments() {
        let (p, _) = Persistence::new_in_memory().await;

        let bar_id = add_service_named(&p.pool, "bar").await.unwrap();
        let foo_id = add_service_named(&p.pool, "foo").await.unwrap();
        let service_id = add_service(&p.pool).await.unwrap();
        let service_id2 = add_service(&p.pool).await.unwrap();

        let id_1 = Uuid::new_v4();
        let id_2 = Uuid::new_v4();
        let id_3 = Uuid::new_v4();

        for deployment in [
            Deployment {
                id: Uuid::new_v4(),
                service_id,
                state: State::Built,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 29, 33).unwrap(),
                address: None,
            },
            Deployment {
                id: id_1,
                service_id: foo_id,
                state: State::Running,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 29, 44).unwrap(),
                address: None,
            },
            Deployment {
                id: id_2,
                service_id: bar_id,
                state: State::Running,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 33, 48).unwrap(),
                address: None,
            },
            Deployment {
                id: Uuid::new_v4(),
                service_id: service_id2,
                state: State::Crashed,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 38, 52).unwrap(),
                address: None,
            },
            Deployment {
                id: id_3,
                service_id: foo_id,
                state: State::Running,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 42, 32).unwrap(),
                address: None,
            },
        ] {
            p.insert_deployment(deployment).await.unwrap();
        }

        let runnable = p.get_all_runnable_deployments().await.unwrap();
        assert_eq!(
            runnable,
            [
                DeploymentRunnable {
                    id: id_1,
                    service_name: "foo".to_string(),
                    service_id: foo_id,
                },
                DeploymentRunnable {
                    id: id_2,
                    service_name: "bar".to_string(),
                    service_id: bar_id,
                },
                DeploymentRunnable {
                    id: id_3,
                    service_name: "foo".to_string(),
                    service_id: foo_id,
                },
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_insert() {
        let (p, _) = Persistence::new_in_memory().await;
        let deployment_id = add_deployment(&p.pool).await.unwrap();

        let log = Log {
            id: deployment_id,
            timestamp: Utc::now(),
            state: State::Queued,
            level: Level::Info,
            file: Some("queue.rs".to_string()),
            line: Some(12),
            target: "tests::log_insert".to_string(),
            fields: json!({"message": "job queued"}),
        };

        insert_log(&p.pool, log.clone()).await.unwrap();

        let logs = p.get_deployment_logs(&deployment_id).await.unwrap();
        assert!(!logs.is_empty(), "there should be one log");

        assert_eq!(logs.first().unwrap(), &log);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn logs_for_deployment() {
        let (p, _) = Persistence::new_in_memory().await;
        let deployment_a = add_deployment(&p.pool).await.unwrap();
        let deployment_b = add_deployment(&p.pool).await.unwrap();

        let log_a1 = Log {
            id: deployment_a,
            timestamp: Utc::now(),
            state: State::Queued,
            level: Level::Info,
            file: Some("file.rs".to_string()),
            line: Some(5),
            target: "tests::logs_for_deployment".to_string(),
            fields: json!({"message": "job queued"}),
        };
        let log_b = Log {
            id: deployment_b,
            timestamp: Utc::now(),
            state: State::Queued,
            level: Level::Info,
            file: Some("file.rs".to_string()),
            line: Some(5),
            target: "tests::logs_for_deployment".to_string(),
            fields: json!({"message": "job queued"}),
        };
        let log_a2 = Log {
            id: deployment_a,
            timestamp: Utc::now(),
            state: State::Building,
            level: Level::Warn,
            file: None,
            line: None,
            target: String::new(),
            fields: json!({"message": "unused Result"}),
        };

        for log in [log_a1.clone(), log_b, log_a2.clone()] {
            insert_log(&p.pool, log).await.unwrap();
        }

        let logs = p.get_deployment_logs(&deployment_a).await.unwrap();
        assert!(!logs.is_empty(), "there should be two logs");

        assert_eq!(logs, vec![log_a1, log_a2]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_recorder_event() {
        let (p, handle) = Persistence::new_in_memory().await;
        let deployment_id = add_deployment(&p.pool).await.unwrap();

        let event = deploy_layer::Log {
            id: deployment_id,
            timestamp: Utc::now(),
            state: State::Queued,
            level: Level::Info,
            file: Some("file.rs".to_string()),
            line: Some(5),
            target: "tests::log_recorder_event".to_string(),
            fields: json!({"message": "job queued"}),
            r#type: deploy_layer::LogType::Event,
            address: None,
        };

        p.record(event);

        // Drop channel and wait for it to finish
        drop(p.log_send);
        assert!(handle.await.is_ok());

        let logs = get_deployment_logs(&p.pool, &deployment_id).await.unwrap();

        assert!(!logs.is_empty(), "there should be one log");

        let log = logs.first().unwrap();
        assert_eq!(log.id, deployment_id);
        assert_eq!(log.state, State::Queued);
        assert_eq!(log.level, Level::Info);
        assert_eq!(log.file, Some("file.rs".to_string()));
        assert_eq!(log.line, Some(5));
        assert_eq!(log.fields, json!({"message": "job queued"}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_recorder_state() {
        let (p, handle) = Persistence::new_in_memory().await;

        let id = Uuid::new_v4();
        let service_id = add_service(&p.pool).await.unwrap();

        p.insert_deployment(Deployment {
            id,
            service_id,
            state: State::Queued, // Should be different from the state recorded below
            last_update: Utc.with_ymd_and_hms(2022, 4, 29, 2, 39, 39).unwrap(),
            address: None,
        })
        .await
        .unwrap();
        let state = deploy_layer::Log {
            id,
            timestamp: Utc.with_ymd_and_hms(2022, 4, 29, 2, 39, 59).unwrap(),
            state: State::Running,
            level: Level::Info,
            file: None,
            line: None,
            target: String::new(),
            fields: serde_json::Value::Null,
            r#type: deploy_layer::LogType::State,
            address: Some("127.0.0.1:12345".to_string()),
        };

        p.record(state);

        // Drop channel and wait for it to finish
        drop(p.log_send);
        assert!(handle.await.is_ok());

        let logs = get_deployment_logs(&p.pool, &id).await.unwrap();

        assert!(!logs.is_empty(), "state change should be logged");

        let log = logs.first().unwrap();
        assert_eq!(log.id, id);
        assert_eq!(log.state, State::Running);
        assert_eq!(log.level, Level::Info);
        assert_eq!(log.fields, json!("NEW STATE"));

        assert_eq!(
            get_deployment(&p.pool, &id).await.unwrap().unwrap(),
            Deployment {
                id,
                service_id,
                state: State::Running,
                last_update: Utc.with_ymd_and_hms(2022, 4, 29, 2, 39, 59).unwrap(),
                address: Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 12345)),
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deployment_resources() {
        let (p, _) = Persistence::new_in_memory().await;
        let service_id = add_service(&p.pool).await.unwrap();
        let service_id2 = add_service(&p.pool).await.unwrap();

        let resource1 = Resource {
            service_id,
            r#type: ResourceType::Database(resource::DatabaseType::Shared(
                resource::database::SharedType::Postgres,
            )),
            data: json!({"username": "root"}),
        };
        let resource2 = Resource {
            service_id,
            r#type: ResourceType::Database(resource::DatabaseType::AwsRds(
                resource::database::AwsRdsType::MariaDB,
            )),
            data: json!({"uri": "postgres://localhost"}),
        };
        let resource3 = Resource {
            service_id: service_id2,
            r#type: ResourceType::Database(resource::DatabaseType::AwsRds(
                resource::database::AwsRdsType::Postgres,
            )),
            data: json!({"username": "admin"}),
        };
        // This makes sure only the last instance of a type is saved (clashes with [resource1])
        let resource4 = Resource {
            service_id,
            r#type: ResourceType::Database(resource::DatabaseType::Shared(
                resource::database::SharedType::Postgres,
            )),
            data: json!({"username": "foo"}),
        };

        for resource in [&resource1, &resource2, &resource3, &resource4] {
            p.insert_resource(resource).await.unwrap();
        }

        let resources = p.get_resources(&service_id).await.unwrap();

        assert_eq!(resources, vec![resource2, resource4]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn secrets() {
        let (p, _) = Persistence::new_in_memory().await;

        let service_id = add_service(&p.pool).await.unwrap();
        let service_id2 = add_service(&p.pool).await.unwrap();

        p.insert_secret(&service_id, "key1", "value1")
            .await
            .unwrap();
        p.insert_secret(&service_id2, "key2", "value2")
            .await
            .unwrap();
        p.insert_secret(&service_id, "key3", "value3")
            .await
            .unwrap();
        p.insert_secret(&service_id, "key1", "value1_updated")
            .await
            .unwrap();

        let actual: Vec<_> = p
            .get_secrets(&service_id)
            .await
            .unwrap()
            .into_iter()
            .map(|mut i| {
                // Reset dates for test
                i.last_update = Default::default();
                i
            })
            .collect();
        let expected = vec![
            Secret {
                service_id,
                key: "key1".to_string(),
                value: "value1_updated".to_string(),
                last_update: Default::default(),
            },
            Secret {
                service_id,
                key: "key3".to_string(),
                value: "value3".to_string(),
                last_update: Default::default(),
            },
        ];

        assert_eq!(actual, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn service() {
        let (p, _) = Persistence::new_in_memory().await;

        let service = p.get_or_create_service("dummy-service").await.unwrap();
        let service2 = p.get_or_create_service("dummy-service").await.unwrap();

        assert_eq!(service, service2, "service should only be added once");

        let get_result = p
            .get_service_by_name("dummy-service")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(service, get_result);

        p.delete_service(&service.id).await.unwrap();
        assert!(p
            .get_service_by_name("dummy-service")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn address_getter() {
        let (p, _) = Persistence::new_in_memory().await;
        let service_id = add_service_named(&p.pool, "service-name").await.unwrap();
        let service_other_id = add_service_named(&p.pool, "other-name").await.unwrap();

        sqlx::query(
            "INSERT INTO deployments (id, service_id, state, last_update, address) VALUES (?, ?, ?, ?, ?), (?, ?, ?, ?, ?), (?, ?, ?, ?, ?)",
        )
        // This running item should match
        .bind(Uuid::new_v4())
        .bind(service_id)
        .bind(State::Running)
        .bind(Utc::now())
        .bind("10.0.0.5:12356")
        // A stopped item should not match
        .bind(Uuid::new_v4())
        .bind(service_id)
        .bind(State::Stopped)
        .bind(Utc::now())
        .bind("10.0.0.5:9876")
        // Another service should not match
        .bind(Uuid::new_v4())
        .bind(service_other_id)
        .bind(State::Running)
        .bind(Utc::now())
        .bind("10.0.0.5:5678")
        .execute(&p.pool)
        .await
        .unwrap();

        assert_eq!(
            SocketAddr::from(([10, 0, 0, 5], 12356)),
            p.get_address_for_service("service-name")
                .await
                .unwrap()
                .unwrap(),
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn active_deployment_getter() {
        let (p, _) = Persistence::new_in_memory().await;
        let service_id = add_service_named(&p.pool, "service-name").await.unwrap();
        let id_1 = Uuid::new_v4();
        let id_2 = Uuid::new_v4();

        for deployment in [
            Deployment {
                id: Uuid::new_v4(),
                service_id,
                state: State::Built,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 29, 33).unwrap(),
                address: None,
            },
            Deployment {
                id: Uuid::new_v4(),
                service_id,
                state: State::Stopped,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 29, 44).unwrap(),
                address: None,
            },
            Deployment {
                id: id_1,
                service_id,
                state: State::Running,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 33, 48).unwrap(),
                address: None,
            },
            Deployment {
                id: Uuid::new_v4(),
                service_id,
                state: State::Crashed,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 38, 52).unwrap(),
                address: None,
            },
            Deployment {
                id: id_2,
                service_id,
                state: State::Running,
                last_update: Utc.with_ymd_and_hms(2022, 4, 25, 4, 42, 32).unwrap(),
                address: None,
            },
        ] {
            p.insert_deployment(deployment).await.unwrap();
        }

        let actual = p.get_active_deployments(&service_id).await.unwrap();

        assert_eq!(actual, vec![id_1, id_2]);
    }

    async fn add_deployment(pool: &SqlitePool) -> Result<Uuid> {
        let service_id = add_service(pool).await?;
        let deployment_id = Uuid::new_v4();

        sqlx::query(
            "INSERT INTO deployments (id, service_id, state, last_update) VALUES (?, ?, ?, ?)",
        )
        .bind(deployment_id)
        .bind(service_id)
        .bind(State::Running)
        .bind(Utc::now())
        .execute(pool)
        .await?;

        Ok(deployment_id)
    }

    async fn add_service(pool: &SqlitePool) -> Result<Uuid> {
        add_service_named(pool, &get_random_name()).await
    }

    async fn add_service_named(pool: &SqlitePool, name: &str) -> Result<Uuid> {
        let service_id = Uuid::new_v4();

        sqlx::query("INSERT INTO services (id, name) VALUES (?, ?)")
            .bind(service_id)
            .bind(name)
            .execute(pool)
            .await?;

        Ok(service_id)
    }

    fn get_random_name() -> String {
        rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(12)
            .map(char::from)
            .collect::<String>()
    }
}
