use super::{
    backend::BackendMonitor,
    engine::{Engine, EngineBackendStatus},
};
use crate::{
    agent::wait_port_ready,
    database::{Backend, DroneDatabase},
};
use anyhow::{anyhow, Result};
use chrono::Utc;
use dashmap::DashMap;
use plane_core::{
    messages::agent::{BackendState, BackendStateMessage, SpawnRequest, TerminationRequest},
    nats::TypedNats,
    types::{BackendId, ClusterName},
};
use serde_json::json;
use std::{fmt::Debug, net::IpAddr, sync::Arc};
use tokio::{
    sync::mpsc::{channel, Sender},
    task::JoinHandle,
};
use tokio_stream::StreamExt;

trait LogError {
    fn log_error(&self) -> &Self;
}

impl<T, E: Debug> LogError for Result<T, E> {
    fn log_error(&self) -> &Self {
        match self {
            Ok(_) => (),
            Err(error) => tracing::error!(?error, "Encountered non-blocking error."),
        }

        self
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Signal {
    /// Tells the executor to interrupt current step to recapture an external status
    /// change. This signal is sent after an engine detects an external status change,
    /// e.g. a backend has terminated itself with an error.
    Interrupt,

    /// Tells the executor to terminate the current step.
    Terminate,
}

pub struct Executor<E: Engine> {
    engine: Arc<E>,
    database: DroneDatabase,
    nc: TypedNats,
    _container_events_handle: Arc<JoinHandle<()>>,

    /// Associates a backend with a monitor, which owns a number of
    /// event loops related to a backend.
    backend_to_monitor: Arc<DashMap<BackendId, BackendMonitor>>,

    /// Associates a backend with a channel, through which signals can
    /// be sent to interrupt the state machine. This is used for
    /// telling the state machine to receive external events, and also
    /// for terminating backends.
    backend_to_listener: Arc<DashMap<BackendId, Sender<Signal>>>,

    /// The IP address associated with this executor.
    ip: IpAddr,

    /// The cluster name associated with this executor.
    cluster: ClusterName,
}

impl<E: Engine> Clone for Executor<E> {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine.clone(),
            database: self.database.clone(),
            nc: self.nc.clone(),
            _container_events_handle: self._container_events_handle.clone(),
            backend_to_monitor: self.backend_to_monitor.clone(),
            backend_to_listener: self.backend_to_listener.clone(),
            ip: self.ip,
            cluster: self.cluster.clone(),
        }
    }
}

impl<E: Engine> Executor<E> {
    pub fn new(
        engine: E,
        database: DroneDatabase,
        nc: TypedNats,
        ip: IpAddr,
        cluster: ClusterName,
    ) -> Self {
        let backend_to_listener: Arc<DashMap<BackendId, Sender<Signal>>> = Arc::default();
        let engine = Arc::new(engine);

        let container_events_handle = tokio::spawn(Self::listen_for_container_events(
            engine.clone(),
            backend_to_listener.clone(),
        ));

        Executor {
            engine,
            database,
            nc,
            _container_events_handle: Arc::new(container_events_handle),
            backend_to_monitor: Arc::default(),
            backend_to_listener,
            ip,
            cluster,
        }
    }

    async fn listen_for_container_events(
        engine: Arc<E>,
        backend_to_listener: Arc<DashMap<BackendId, Sender<Signal>>>,
    ) {
        let mut event_stream = engine.interrupt_stream();
        while let Some(backend_id) = event_stream.next().await {
            if let Some(v) = backend_to_listener.get(&backend_id) {
                v.try_send(Signal::Interrupt).log_error();
            }
        }
    }

    pub async fn start_backend(&self, spawn_request: &SpawnRequest) {
        self.database
            .insert_backend(spawn_request)
            .await
            .log_error();

        self.nc
            .publish_jetstream(&BackendStateMessage::new(
                BackendState::Loading,
                spawn_request.backend_id.clone(),
            ))
            .await
            .log_error();

        self.run_backend(spawn_request, BackendState::Loading).await
    }

    pub async fn kill_backend(
        &self,
        termination_request: &TerminationRequest,
    ) -> Result<(), anyhow::Error> {
        if let Some(sender) = self
            .backend_to_listener
            .get(&termination_request.backend_id)
        {
            Ok(sender.send(Signal::Terminate).await?)
        } else {
            Err(anyhow!(
                "Unknown backend {}",
                &termination_request.backend_id
            ))
        }
    }

    pub async fn resume_backends(&self) -> Result<()> {
        let backends = self.database.get_backends().await?;

        for backend in backends {
            let executor = self.clone();
            let Backend {
                backend_id,
                state,
                spec,
            } = backend;
            tracing::info!(%backend_id, ?state, "Resuming backend");

            if state.running() {
                self.backend_to_monitor.insert(
                    backend_id.clone(),
                    BackendMonitor::new(
                        &backend_id,
                        &self.cluster,
                        self.ip,
                        self.engine.as_ref(),
                        &self.nc,
                    ),
                );
            }
            tokio::spawn(async move { executor.run_backend(&spec, state).await });
        }

        Ok(())
    }

    async fn run_backend(&self, spawn_request: &SpawnRequest, mut state: BackendState) {
        let (send, mut recv) = channel(1);
        self.backend_to_listener
            .insert(spawn_request.backend_id.clone(), send);

        if spawn_request.bearer_token.is_some() {
            tracing::warn!(
                "Spawn request included bearer token, which is not currently supported."
            );
        }

        loop {
            tracing::info!(
                ?state,
                backend_id = spawn_request.backend_id.id(),
                metadata = %json!(spawn_request.metadata),
                "Executing state."
            );

            let next_state = loop {
                if state == BackendState::Swept {
                    // When sweeping, we ignore external state changes to avoid an infinite loop.
                    break self.step(spawn_request, state).await;
                } else {
                    // Otherwise, we allow the step to be interrupted if the state changes (i.e.
                    // if the container dies).
                    tokio::select! {
                        next_state = self.step(spawn_request, state) => break next_state,
                        sig = recv.recv() => match sig {
                            Some(Signal::Interrupt) => {
                                tracing::info!("State may have updated externally.");
                                continue;
                            },
                            Some(Signal::Terminate) => {
                                break Ok(Some(BackendState::Terminated))
                            },
                            None => {
                                tracing::error!("Signal sender lost!");
                                return
                            }
                        },
                    }
                };
            };

            match next_state {
                Ok(Some(new_state)) => {
                    state = new_state;

                    if state.running() {
                        self.backend_to_monitor.insert(
                            spawn_request.backend_id.clone(),
                            BackendMonitor::new(
                                &spawn_request.backend_id,
                                &self.cluster,
                                self.ip,
                                self.engine.as_ref(),
                                &self.nc,
                            ),
                        );
                    }

                    self.update_backend_state(spawn_request, state).await;
                }
                Ok(None) => {
                    // Successful termination.
                    tracing::info!("Terminated successfully.");
                    break;
                }
                Err(error) => {
                    tracing::error!(?error, ?state, "Encountered error.");
                    match state {
                        BackendState::Loading => {
                            state = BackendState::ErrorLoading;
                            self.update_backend_state(spawn_request, state).await;
                        }
                        _ => tracing::error!(
                            ?error,
                            ?state,
                            "Error unhandled (no change in backend state)"
                        ),
                    }
                    break;
                }
            }
        }

        self.backend_to_monitor.remove(&spawn_request.backend_id);
        self.backend_to_listener.remove(&spawn_request.backend_id);
    }

    /// Update the rest of the system on the state of a backend, by writing it to the local
    /// sqlite database (where the proxy can see it), and by broadcasting it to interested
    /// remote listeners over NATS.
    async fn update_backend_state(&self, spawn_request: &SpawnRequest, state: BackendState) {
        self.database
            .update_backend_state(&spawn_request.backend_id, state)
            .await
            .log_error();

        self.nc
            .publish_jetstream(&BackendStateMessage::new(
                state,
                spawn_request.backend_id.clone(),
            ))
            .await
            .log_error();
    }

    pub async fn step(
        &self,
        spawn_request: &SpawnRequest,
        state: BackendState,
    ) -> Result<Option<BackendState>> {
        match state {
            BackendState::Loading => {
                self.engine.load(spawn_request).await?;

                Ok(Some(BackendState::Starting))
            }
            BackendState::Starting => {
                let status = self
                    .engine
                    .backend_status(&spawn_request.backend_id)
                    .await?;

                let backend_addr = match status {
                    EngineBackendStatus::Running { addr } => addr,
                    _ => return Ok(Some(BackendState::ErrorStarting)),
                };

                tracing::info!(%backend_addr, "Got address from container.");
                wait_port_ready(&backend_addr).await?;

                self.database
                    .insert_proxy_route(
                        &spawn_request.backend_id,
                        spawn_request.backend_id.id(),
                        &backend_addr.to_string(),
                    )
                    .await?;

                Ok(Some(BackendState::Ready))
            }
            BackendState::Ready => {
                match self
                    .engine
                    .backend_status(&spawn_request.backend_id)
                    .await?
                {
                    EngineBackendStatus::Failed => return Ok(Some(BackendState::Failed)),
                    EngineBackendStatus::Exited => return Ok(Some(BackendState::Exited)),
                    EngineBackendStatus::Terminated => return Ok(Some(BackendState::Swept)),
                    _ => (),
                }

                // wait for idle
                loop {
                    let last_active = self
                        .database
                        .get_backend_last_active(&spawn_request.backend_id)
                        .await?;
                    let next_check = last_active
                        .checked_add_signed(chrono::Duration::from_std(
                            spawn_request.max_idle_secs,
                        )?)
                        .ok_or_else(|| anyhow!("Checked add error."))?;

                    if next_check < Utc::now() {
                        break;
                    } else {
                        tokio::time::sleep(next_check.signed_duration_since(Utc::now()).to_std()?)
                            .await;
                    }
                }

                Ok(Some(BackendState::Swept))
            }
            BackendState::ErrorLoading
            | BackendState::ErrorStarting
            | BackendState::TimedOutBeforeReady
            | BackendState::Failed
            | BackendState::Exited
            | BackendState::Swept
            | BackendState::Terminated => {
                self.engine
                    .stop(&spawn_request.backend_id)
                    .await
                    .map_err(|e| anyhow!("Error stopping container: {:?}", e))?;

                Ok(None)
            }
        }
    }
}
