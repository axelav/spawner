use anyhow::{anyhow, Result};
use bollard::container::Stats;
use bollard::service::{ContainerInspectResponse, EventMessage};
use futures::Stream;
use plane_core::{messages::agent::BackendStatsMessage, types::BackendId};
use std::{collections::HashMap, net::IpAddr, pin::Pin, task::Poll};

pub trait MinuteExt {
    fn as_minutes(&self) -> u128;
}

impl MinuteExt for std::time::Duration {
    fn as_minutes(&self) -> u128 {
        (self.as_secs() / 60).into()
    }
}

pub fn make_exposed_ports(port: u16) -> Option<HashMap<String, HashMap<(), ()>>> {
    let dummy: HashMap<(), ()> = vec![].into_iter().collect();
    Some(vec![(format!("{}/tcp", port), dummy)].into_iter().collect())
}

/// Helper trait for swallowing Docker not found errors.
pub trait AllowNotFound {
    /// Swallow a result if it is a success result or a NotFound; propagate it otherwise.
    fn allow_not_found(self) -> Result<(), bollard::errors::Error>;
}

impl<T> AllowNotFound for Result<T, bollard::errors::Error> {
    fn allow_not_found(self) -> Result<(), bollard::errors::Error> {
        match self {
            Ok(_) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                message,
            }) => {
                tracing::warn!(
                    ?message,
                    "Received 404 error from docker, possibly expected."
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// The list of possible container events.
/// Comes from [Docker documentation](https://docs.docker.com/engine/reference/commandline/events/).
#[derive(Debug, PartialEq, Eq)]
pub enum ContainerEventType {
    Attach,
    Commit,
    Copy,
    Create,
    Destroy,
    Detach,
    Die,
    ExecCreate,
    ExecDetach,
    ExecDie,
    ExecStart,
    Export,
    HealthStatus,
    Kill,
    Oom,
    Pause,
    Rename,
    Resize,
    Restart,
    Start,
    Stop,
    Top,
    Unpause,
    Update,
}

#[allow(unused)]
#[derive(Debug)]
pub struct ContainerEvent {
    pub event: ContainerEventType,
    pub name: String,
}

impl ContainerEvent {
    pub fn from_event_message(event: &EventMessage) -> Option<Self> {
        let action = event.action.as_deref()?;
        let actor = event.actor.as_ref()?;
        let name: String = actor.attributes.as_ref()?.get("name")?.to_string();
        // Some actions add extra metadata after a colon, we strip that out.
        let action = action.split(':').next().expect("First next() on split should never fail.");

        let event = match action {
            "attach" => ContainerEventType::Attach,
            "commit" => ContainerEventType::Commit,
            "copy" => ContainerEventType::Copy,
            "create" => ContainerEventType::Create,
            "destroy" => ContainerEventType::Destroy,
            "detach" => ContainerEventType::Detach,
            "die" => ContainerEventType::Die,
            "exec_create" => ContainerEventType::ExecCreate,
            "exec_detach" => ContainerEventType::ExecDetach,
            "exec_die" => ContainerEventType::ExecDie,
            "exec_start" => ContainerEventType::ExecStart,
            "export" => ContainerEventType::Export,
            "health_status" => ContainerEventType::HealthStatus,
            "kill" => ContainerEventType::Kill,
            "oom" => ContainerEventType::Oom,
            "pause" => ContainerEventType::Pause,
            "rename" => ContainerEventType::Rename,
            "resize" => ContainerEventType::Resize,
            "restart" => ContainerEventType::Restart,
            "start" => ContainerEventType::Start,
            "stop" => ContainerEventType::Stop,
            "top" => ContainerEventType::Top,
            "unpause" => ContainerEventType::Unpause,
            "update" => ContainerEventType::Update,
            _ => {
                tracing::info!(?action, "Unhandled container action.");
                return None;
            }
        };

        Some(ContainerEvent { event, name })
    }
}

pub fn get_ip_of_container(inspect_response: &ContainerInspectResponse) -> Result<IpAddr> {
    let network_settings = inspect_response
        .network_settings
        .as_ref()
        .ok_or_else(|| anyhow!("Inspect did not return network settings."))?;

    if let Some(ip_addr) = network_settings.ip_address.as_ref() {
        if !ip_addr.is_empty() {
            return Ok(ip_addr.parse()?);
        }
    }

    let networks = network_settings
        .networks
        .as_ref()
        .ok_or_else(|| anyhow!("Inspect did not return an IP or networks."))?;
    if networks.len() != 1 {
        return Err(anyhow!(
            "Expected exactly one network, got {}",
            networks.len()
        ));
    }

    let network = networks
        .values()
        .into_iter()
        .next()
        .expect("next() should never fail after length check.");

    let ip = network
        .ip_address
        .as_ref()
        .ok_or_else(|| anyhow!("One network found, but did not have IP address."))?;

    Ok(ip.parse()?)
}

pub struct StatsStream<T: Stream<Item = Stats> + Unpin> {
    stream: T,
    last: Option<Stats>,
    backend_id: BackendId,
}

impl<T: Stream<Item = Stats> + Unpin> StatsStream<T> {
    pub fn new(backend_id: BackendId, stream: T) -> StatsStream<T> {
        StatsStream {
            stream,
            last: None,
            backend_id,
        }
    }
}

impl<T: Stream<Item = Stats> + Unpin> Stream for StatsStream<T> {
    type Item = BackendStatsMessage;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let next = futures::StreamExt::poll_next_unpin(&mut self.stream, cx);

        match next {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(stat)) => {
                if let Some(last) = &self.last {
                    let v = BackendStatsMessage::from_stats_messages(&self.backend_id, last, &stat)
                        .unwrap();

                    self.last = Some(stat);
                    Poll::Ready(Some(v))
                } else {
                    self.last = Some(stat);
                    Poll::Pending
                }
            }
        }
    }
}
