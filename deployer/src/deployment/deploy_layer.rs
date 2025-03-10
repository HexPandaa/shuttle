//! This is a layer for [tracing] to capture the state transition of deploys
//!
//! The idea is as follow: as a deployment moves through the [super::DeploymentManager] a set of functions will be invoked.
//! These functions are clear markers for the deployment entering a new state so we would want to change the state as soon as entering these functions.
//! But rather than passing a persistence layer around to be able record the state in these functions we can rather use [tracing].
//!
//! This is very similar to Aspect Oriented Programming where we use the annotations from the function to trigger the recording of a new state.
//! This annotation is a [#[instrument]](https://docs.rs/tracing-attributes/latest/tracing_attributes/attr.instrument.html) with an `id` and `state` field as follow:
//! ```no-test
//! #[instrument(fields(id = %built.id, state = %State::Built))]
//! pub async fn new_state_fn(built: Built) {
//!     // Get built ready for starting
//! }
//! ```
//!
//! Here the `id` is extracted from the `built` argument and the `state` is taken from the [State] enum (the special `%` is needed to use the `Display` trait to convert the values to a str).
//!
//! All `debug!()` etc in these functions will be captured by this layer and will be associated with the deployment and the state.
//!
//! **Warning** Don't log out sensitive info in functions with these annotations

use chrono::{DateTime, Utc};
use serde_json::json;
use shuttle_common::STATE_MESSAGE;
use std::{net::SocketAddr, str::FromStr};
use tracing::{error, field::Visit, span, warn, Metadata, Subscriber};
use tracing_subscriber::Layer;
use uuid::Uuid;

use crate::persistence::{self, DeploymentState, LogLevel, State};

/// Records logs for the deployment progress
pub trait LogRecorder: Clone + Send + 'static {
    fn record(&self, log: Log);
}

/// An event or state transition log
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Log {
    /// Deployment id
    pub id: Uuid,

    /// Current state of the deployment
    pub state: State,

    /// Log level
    pub level: LogLevel,

    /// Time log happened
    pub timestamp: DateTime<Utc>,

    /// File event took place in
    pub file: Option<String>,

    /// Line in file event happened on
    pub line: Option<u32>,

    /// Module log took place in
    pub target: String,

    /// Extra structured log fields
    pub fields: serde_json::Value,

    pub r#type: LogType,

    pub address: Option<String>,
}

impl From<Log> for persistence::Log {
    fn from(log: Log) -> Self {
        // Make sure state message is set for state logs
        // This is used to know when the end of the build logs has been reached
        let fields = match log.r#type {
            LogType::Event => log.fields,
            LogType::State => json!(STATE_MESSAGE),
        };

        Self {
            id: log.id,
            timestamp: log.timestamp,
            state: log.state,
            level: log.level,
            file: log.file,
            line: log.line,
            target: log.target,
            fields,
        }
    }
}

impl From<Log> for shuttle_common::LogItem {
    fn from(log: Log) -> Self {
        Self {
            id: log.id,
            timestamp: log.timestamp,
            state: log.state.into(),
            level: log.level.into(),
            file: log.file,
            line: log.line,
            target: log.target,
            fields: serde_json::to_vec(&log.fields).unwrap(),
        }
    }
}

impl From<Log> for DeploymentState {
    fn from(log: Log) -> Self {
        let address = if let Some(address_str) = log.address {
            match SocketAddr::from_str(&address_str) {
                Ok(address) => Some(address),
                Err(err) => {
                    error!(error = %err, "failed to convert to [SocketAddr]");
                    None
                }
            }
        } else {
            None
        };

        Self {
            id: log.id,
            state: log.state,
            last_update: log.timestamp,
            address,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogType {
    Event,
    State,
}

/// Tracing subscriber layer which keeps track of a deployment's state
pub struct DeployLayer<R>
where
    R: LogRecorder + Send + Sync,
{
    recorder: R,
}

impl<R> DeployLayer<R>
where
    R: LogRecorder + Send + Sync,
{
    pub fn new(recorder: R) -> Self {
        Self { recorder }
    }
}

impl<R, S> Layer<S> for DeployLayer<R>
where
    S: Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
    R: LogRecorder + Send + Sync + 'static,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        // We only care about events in some state scope
        let scope = if let Some(scope) = ctx.event_scope(event) {
            scope
        } else {
            return;
        };

        // Find the first scope with the scope details containing the current state
        for span in scope.from_root() {
            let extensions = span.extensions();

            if let Some(details) = extensions.get::<ScopeDetails>() {
                let mut visitor = JsonVisitor::default();

                event.record(&mut visitor);
                let metadata = event.metadata();

                // Extract details from log::Log interface which is different from tracing
                let target = if let Some(target) = visitor.0.remove("log.target") {
                    target.as_str().unwrap_or_default().to_string()
                } else {
                    metadata.target().to_string()
                };

                let line = if let Some(line) = visitor.0.remove("log.line") {
                    line.as_u64().and_then(|u| u32::try_from(u).ok())
                } else {
                    metadata.line()
                };

                let file = if let Some(file) = visitor.0.remove("log.file") {
                    file.as_str().map(str::to_string)
                } else {
                    metadata.file().map(str::to_string)
                };

                visitor.0.remove("log.module_path");

                self.recorder.record(Log {
                    id: details.id,
                    state: details.state,
                    level: metadata.level().into(),
                    timestamp: Utc::now(),
                    file,
                    line,
                    target,
                    fields: serde_json::Value::Object(visitor.0),
                    r#type: LogType::Event,
                    address: None,
                });
                break;
            }
        }
    }

    fn on_new_span(
        &self,
        attrs: &span::Attributes<'_>,
        id: &span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // We only care about spans that change the state
        if !NewStateVisitor::is_valid(attrs.metadata()) {
            return;
        }

        let mut visitor = NewStateVisitor::default();

        attrs.record(&mut visitor);

        let details = visitor.details;

        if details.id.is_nil() {
            warn!("scope details does not have a valid id");
            return;
        }

        // Safe to unwrap since this is the `on_new_span` method
        let span = ctx.span(id).unwrap();
        let mut extensions = span.extensions_mut();
        let metadata = span.metadata();

        self.recorder.record(Log {
            id: details.id,
            state: details.state,
            level: metadata.level().into(),
            timestamp: Utc::now(),
            file: metadata.file().map(str::to_string),
            line: metadata.line(),
            target: metadata.target().to_string(),
            fields: Default::default(),
            r#type: LogType::State,
            address: details.address.clone(),
        });

        extensions.insert::<ScopeDetails>(details);
    }
}

/// Used to keep track of the current state a deployment scope is in
#[derive(Debug, Default)]
struct ScopeDetails {
    id: Uuid,
    state: State,
    address: Option<String>,
}

impl From<&tracing::Level> for LogLevel {
    fn from(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::TRACE => Self::Trace,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::INFO => Self::Info,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::ERROR => Self::Error,
        }
    }
}

/// This visitor is meant to extract the `ScopeDetails` for any scope with `name` and `status` fields
#[derive(Default)]
struct NewStateVisitor {
    details: ScopeDetails,
}

impl NewStateVisitor {
    /// Field containing the deployment identifier
    const ID_IDENT: &'static str = "id";

    /// Field containing the deployment state identifier
    const STATE_IDENT: &'static str = "state";

    /// Field containing the deployment address identifier
    const ADDRESS_IDENT: &'static str = "address";

    fn is_valid(metadata: &Metadata) -> bool {
        metadata.is_span()
            && metadata.fields().field(Self::ID_IDENT).is_some()
            && metadata.fields().field(Self::STATE_IDENT).is_some()
    }
}

impl Visit for NewStateVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == Self::STATE_IDENT {
            self.details.state = State::from_str(&format!("{value:?}")).unwrap_or_default();
        } else if field.name() == Self::ID_IDENT {
            self.details.id = Uuid::try_parse(&format!("{value:?}")).unwrap_or_default();
        } else if field.name() == Self::ADDRESS_IDENT {
            self.details.address = Some(format!("{value:?}"));
        }
    }
}

#[derive(Default)]
struct JsonVisitor(serde_json::Map<String, serde_json::Value>);

impl Visit for JsonVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.0
            .insert(field.name().to_string(), json!(value.to_string()));
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), json!(format!("{value:?}")));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs::read_dir,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use axum::body::Bytes;
    use ctor::ctor;
    use flate2::{write::GzEncoder, Compression};
    use shuttle_common::backends::auth::Claim;
    use shuttle_service::Logger;
    use tokio::{select, sync::mpsc, time::sleep};
    use tracing_subscriber::prelude::*;
    use uuid::Uuid;

    use crate::{
        deployment::{
            deploy_layer::LogType, gateway_client::BuildQueueClient, provisioner_factory,
            runtime_logger, storage_manager::StorageManager, ActiveDeploymentsGetter, Built,
            DeploymentManager, Queued,
        },
        persistence::{SecretRecorder, State},
    };

    use super::{DeployLayer, Log, LogRecorder};

    #[ctor]
    static RECORDER: Arc<Mutex<RecorderMock>> = {
        let recorder = RecorderMock::new();
        tracing_subscriber::registry()
            .with(DeployLayer::new(Arc::clone(&recorder)))
            .init();

        recorder
    };

    #[derive(Clone)]
    struct RecorderMock {
        states: Arc<Mutex<Vec<StateLog>>>,
    }

    #[derive(Clone, Debug, PartialEq)]
    struct StateLog {
        id: Uuid,
        state: State,
        has_address: bool,
    }

    impl From<Log> for StateLog {
        fn from(log: Log) -> Self {
            Self {
                id: log.id,
                state: log.state,
                has_address: log.address.is_some(),
            }
        }
    }

    impl RecorderMock {
        fn new() -> Arc<Mutex<Self>> {
            Arc::new(Mutex::new(Self {
                states: Arc::new(Mutex::new(Vec::new())),
            }))
        }

        fn get_deployment_states(&self, id: &Uuid) -> Vec<StateLog> {
            self.states
                .lock()
                .unwrap()
                .iter()
                .filter(|log| log.id == *id)
                .cloned()
                .collect()
        }
    }

    impl LogRecorder for RecorderMock {
        fn record(&self, event: Log) {
            // We are only testing the state transitions
            if event.r#type == LogType::State {
                self.states.lock().unwrap().push(event.into());
            }
        }
    }

    #[async_trait::async_trait]
    impl SecretRecorder for Arc<Mutex<RecorderMock>> {
        type Err = std::io::Error;

        async fn insert_secret(
            &self,
            _service_id: &Uuid,
            _key: &str,
            _value: &str,
        ) -> Result<(), Self::Err> {
            panic!("no tests should set secrets")
        }
    }

    impl<R: LogRecorder> LogRecorder for Arc<Mutex<R>> {
        fn record(&self, event: Log) {
            self.lock().unwrap().record(event);
        }
    }

    struct StubAbstractProvisionerFactory;

    #[async_trait::async_trait]
    impl provisioner_factory::AbstractFactory for StubAbstractProvisionerFactory {
        type Output = StubProvisionerFactory;
        type Error = std::io::Error;

        async fn get_factory(
            &self,
            _project_name: shuttle_common::project::ProjectName,
            _service_id: Uuid,
            _deployment_id: Uuid,
            _storage_manager: StorageManager,
            _claim: Option<Claim>,
        ) -> Result<Self::Output, Self::Error> {
            Ok(StubProvisionerFactory)
        }
    }

    struct StubProvisionerFactory;

    #[async_trait::async_trait]
    impl shuttle_service::Factory for StubProvisionerFactory {
        async fn get_db_connection_string(
            &mut self,
            _db_type: shuttle_common::database::Type,
        ) -> Result<String, shuttle_service::Error> {
            panic!("did not expect any deploy_layer test to connect to the database")
        }

        async fn get_secrets(
            &mut self,
        ) -> Result<BTreeMap<String, String>, shuttle_service::Error> {
            panic!("did not expect any deploy_layer test to get secrets")
        }

        fn get_service_name(&self) -> shuttle_service::ServiceName {
            panic!("did not expect any deploy_layer test to get the service name")
        }

        fn get_environment(&self) -> shuttle_service::Environment {
            panic!("did not expect any deploy_layer test to get the environment")
        }

        fn get_build_path(&self) -> Result<PathBuf, shuttle_service::Error> {
            panic!("did not expect any deploy_layer test to get the build path")
        }

        fn get_storage_path(&self) -> Result<PathBuf, shuttle_service::Error> {
            panic!("did not expect any deploy_layer test to get the storage path")
        }
    }

    struct StubRuntimeLoggerFactory;

    impl runtime_logger::Factory for StubRuntimeLoggerFactory {
        fn get_logger(&self, id: Uuid) -> Logger {
            let (tx, mut rx) = mpsc::unbounded_channel();

            tokio::spawn(async move {
                while let Some(log) = rx.recv().await {
                    println!("{log}")
                }
            });

            Logger::new(tx, id)
        }
    }

    #[derive(Clone)]
    struct StubActiveDeploymentGetter;

    #[async_trait::async_trait]
    impl ActiveDeploymentsGetter for StubActiveDeploymentGetter {
        type Err = std::io::Error;

        async fn get_active_deployments(
            &self,
            _service_id: &Uuid,
        ) -> std::result::Result<Vec<Uuid>, Self::Err> {
            Ok(vec![])
        }
    }

    #[derive(Clone)]
    struct StubBuildQueueClient;

    #[async_trait::async_trait]
    impl BuildQueueClient for StubBuildQueueClient {
        async fn get_slot(
            &self,
            _id: Uuid,
        ) -> Result<bool, crate::deployment::gateway_client::Error> {
            Ok(true)
        }

        async fn release_slot(
            &self,
            _id: Uuid,
        ) -> Result<(), crate::deployment::gateway_client::Error> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deployment_to_be_queued() {
        let deployment_manager = get_deployment_manager();

        let queued = get_queue("sleep-async");
        let id = queued.id;
        deployment_manager.queue_push(queued).await;

        let test = async {
            loop {
                let recorder = RECORDER.lock().unwrap();
                let states = recorder.get_deployment_states(&id);

                if states.len() < 5 {
                    drop(recorder); // Don't block
                    sleep(Duration::from_millis(350)).await;
                    continue;
                }

                assert_eq!(
                    states.len(),
                    5,
                    "did not expect these states:\n\t{states:#?}"
                );

                assert_eq!(
                    *states,
                    vec![
                        StateLog {
                            id,
                            state: State::Queued,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Building,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Built,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Loading,
                            has_address: true,
                        },
                        StateLog {
                            id,
                            state: State::Running,
                            has_address: true,
                        },
                    ]
                );

                break;
            }
        };

        select! {
            _ = sleep(Duration::from_secs(180)) => {
                panic!("states should go into 'Running' for a valid service");
            }
            _ = test => {}
        }

        // Send kill signal
        deployment_manager.kill(id).await;

        sleep(Duration::from_secs(1)).await;

        let recorder = RECORDER.lock().unwrap();
        let states = recorder.get_deployment_states(&id);

        assert_eq!(
            *states,
            vec![
                StateLog {
                    id,
                    state: State::Queued,
                    has_address: false,
                },
                StateLog {
                    id,
                    state: State::Building,
                    has_address: false,
                },
                StateLog {
                    id,
                    state: State::Built,
                    has_address: false,
                },
                StateLog {
                    id,
                    state: State::Loading,
                    has_address: true,
                },
                StateLog {
                    id,
                    state: State::Running,
                    has_address: true,
                },
                StateLog {
                    id,
                    state: State::Stopped,
                    has_address: false,
                },
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deployment_self_stop() {
        let deployment_manager = get_deployment_manager();

        let queued = get_queue("self-stop");
        let id = queued.id;
        deployment_manager.queue_push(queued).await;

        let test = async {
            loop {
                let recorder = RECORDER.lock().unwrap();
                let states = recorder.get_deployment_states(&id);

                if states.len() < 6 {
                    drop(recorder); // Don't block
                    sleep(Duration::from_millis(350)).await;
                    continue;
                }

                assert_eq!(
                    states.len(),
                    6,
                    "did not expect these states:\n\t{states:#?}"
                );

                assert_eq!(
                    *states,
                    vec![
                        StateLog {
                            id,
                            state: State::Queued,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Building,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Built,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Loading,
                            has_address: true,
                        },
                        StateLog {
                            id,
                            state: State::Running,
                            has_address: true,
                        },
                        StateLog {
                            id,
                            state: State::Completed,
                            has_address: false,
                        },
                    ]
                );

                break;
            }
        };

        select! {
            _ = sleep(Duration::from_secs(180)) => {
                panic!("states should go into 'Completed' when a service stops by itself");
            }
            _ = test => {}
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deployment_bind_panic() {
        let deployment_manager = get_deployment_manager();

        let queued = get_queue("bind-panic");
        let id = queued.id;
        deployment_manager.queue_push(queued).await;

        let test = async {
            loop {
                let recorder = RECORDER.lock().unwrap();
                let states = recorder.get_deployment_states(&id);

                if states.len() < 6 {
                    drop(recorder); // Don't block
                    sleep(Duration::from_millis(350)).await;
                    continue;
                }

                assert_eq!(
                    states.len(),
                    6,
                    "did not expect these states:\n\t{states:#?}"
                );

                assert_eq!(
                    *states,
                    vec![
                        StateLog {
                            id,
                            state: State::Queued,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Building,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Built,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Loading,
                            has_address: true,
                        },
                        StateLog {
                            id,
                            state: State::Running,
                            has_address: true,
                        },
                        StateLog {
                            id,
                            state: State::Crashed,
                            has_address: false,
                        },
                    ]
                );

                break;
            }
        };

        select! {
            _ = sleep(Duration::from_secs(180)) => {
                panic!("states should go into 'Crashed' panicing in bind");
            }
            _ = test => {}
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deployment_main_panic() {
        let deployment_manager = get_deployment_manager();

        let queued = get_queue("main-panic");
        let id = queued.id;
        deployment_manager.queue_push(queued).await;

        let test = async {
            loop {
                let recorder = RECORDER.lock().unwrap();
                let states = recorder.get_deployment_states(&id);

                if states.len() < 5 {
                    drop(recorder); // Don't block
                    sleep(Duration::from_millis(350)).await;
                    continue;
                }

                assert_eq!(
                    states.len(),
                    5,
                    "did not expect these states:\n\t{states:#?}"
                );

                assert_eq!(
                    *states,
                    vec![
                        StateLog {
                            id,
                            state: State::Queued,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Building,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Built,
                            has_address: false,
                        },
                        StateLog {
                            id,
                            state: State::Loading,
                            has_address: true,
                        },
                        StateLog {
                            id,
                            state: State::Crashed,
                            has_address: false,
                        },
                    ]
                );

                break;
            }
        };

        select! {
            _ = sleep(Duration::from_secs(180)) => {
                panic!("states should go into 'Crashed' when panicing in main");
            }
            _ = test => {}
        }
    }

    #[tokio::test]
    async fn deployment_from_run() {
        let deployment_manager = get_deployment_manager();

        let id = Uuid::new_v4();
        deployment_manager
            .run_push(Built {
                id,
                service_name: "run-test".to_string(),
                service_id: Uuid::new_v4(),
                tracing_context: Default::default(),
                claim: None,
            })
            .await;

        // Give it a small time to start up
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let recorder = RECORDER.lock().unwrap();
        let states = recorder.get_deployment_states(&id);

        assert_eq!(
            states.len(),
            3,
            "did not expect these states:\n\t{states:#?}"
        );

        assert_eq!(
            *states,
            vec![
                StateLog {
                    id,
                    state: State::Built,
                    has_address: false,
                },
                StateLog {
                    id,
                    state: State::Loading,
                    has_address: true,
                },
                StateLog {
                    id,
                    state: State::Crashed,
                    has_address: false,
                },
            ]
        );
    }

    #[tokio::test]
    async fn scope_with_nil_id() {
        let deployment_manager = get_deployment_manager();

        let id = Uuid::nil();
        deployment_manager
            .queue_push(Queued {
                id,
                service_name: "nil_id".to_string(),
                service_id: Uuid::new_v4(),
                data: Bytes::from("violets are red").to_vec(),
                will_run_tests: false,
                tracing_context: Default::default(),
                claim: None,
            })
            .await;

        // Give it a small time to start up
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let recorder = RECORDER.lock().unwrap();
        let states = recorder.get_deployment_states(&id);

        assert!(
            states.is_empty(),
            "no logs should be recorded when the scope id is invalid:\n\t{states:#?}"
        );
    }

    fn get_deployment_manager() -> DeploymentManager {
        DeploymentManager::builder()
            .abstract_factory(StubAbstractProvisionerFactory)
            .runtime_logger_factory(StubRuntimeLoggerFactory)
            .build_log_recorder(RECORDER.clone())
            .secret_recorder(RECORDER.clone())
            .active_deployment_getter(StubActiveDeploymentGetter)
            .artifacts_path(PathBuf::from("/tmp"))
            .queue_client(StubBuildQueueClient)
            .build()
    }

    fn get_queue(name: &str) -> Queued {
        let enc = GzEncoder::new(Vec::new(), Compression::fast());
        let mut tar = tar::Builder::new(enc);

        for dir_entry in read_dir(format!("tests/deploy_layer/{name}")).unwrap() {
            let dir_entry = dir_entry.unwrap();
            if dir_entry.file_name() != "target" {
                let path = format!("{name}/{}", dir_entry.file_name().to_str().unwrap());

                if dir_entry.file_type().unwrap().is_dir() {
                    tar.append_dir_all(path, dir_entry.path()).unwrap();
                } else {
                    tar.append_path_with_name(dir_entry.path(), path).unwrap();
                }
            }
        }

        let enc = tar.into_inner().unwrap();
        let bytes = enc.finish().unwrap();

        println!("{name}: finished getting archive for test");

        Queued {
            id: Uuid::new_v4(),
            service_name: format!("deploy-layer-{name}"),
            service_id: Uuid::new_v4(),
            data: bytes,
            will_run_tests: false,
            tracing_context: Default::default(),
            claim: None,
        }
    }
}
