use super::agent::{DockerExecutableConfig, SpawnRequest};
use crate::{
    nats::{SubscribeSubject, TypedMessage},
    types::{BackendId, ClusterName, DroneId},
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use serde_with::DurationSeconds;
use std::{collections::HashMap, time::Duration};

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ScheduleRequest {
    pub cluster: ClusterName,

    /// The name of the backend. This forms part of the hostname used to
    /// connect to the drone.
    pub backend_id: Option<BackendId>,

    /// The timeout after which the drone is shut down if no connections are made.
    #[serde_as(as = "DurationSeconds")]
    pub max_idle_secs: Duration,

    /// Metadata for the spawn. Typically added to log messages for debugging and observability.
    pub metadata: HashMap<String, String>,

    /// Configuration for docker run (image, creds, env vars etc.)
    pub executable: DockerExecutableConfig,

    #[serde(default)]
    pub require_bearer_token: bool,
}

impl ScheduleRequest {
    pub fn schedule(&self, drone_id: &DroneId) -> SpawnRequest {
        let backend_id = self
            .backend_id
            .clone()
            .unwrap_or_else(BackendId::new_random);

        if self.require_bearer_token {
            tracing::warn!("Scheduler received request with auth_token, which is not yet implemented. Ignoring.");
        }

        SpawnRequest {
            drone_id: drone_id.clone(),
            backend_id,
            max_idle_secs: self.max_idle_secs,
            metadata: self.metadata.clone(),
            executable: self.executable.clone(),
            bearer_token: None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ScheduleResponse {
    Scheduled {
        drone: DroneId,
        backend_id: BackendId,
        #[serde(skip_serializing_if = "Option::is_none")]
        bearer_token: Option<String>,
    },
    NoDroneAvailable,
}

impl TypedMessage for ScheduleRequest {
    type Response = ScheduleResponse;

    fn subject(&self) -> String {
        format!("cluster.{}.schedule", self.cluster.subject_name())
    }
}

impl ScheduleRequest {
    pub fn subscribe_subject() -> SubscribeSubject<Self> {
        SubscribeSubject::new("cluster.*.schedule".into())
    }
}

/// Message sent to a drone to tell it to start draining.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DrainDrone {
    pub drone: DroneId,
    pub cluster: ClusterName,
    pub drain: bool,
}

impl TypedMessage for DrainDrone {
    type Response = ();

    fn subject(&self) -> String {
        format!(
            "cluster.{}.drone.{}.drain",
            self.cluster.subject_name(),
            self.drone.id()
        )
    }
}

impl DrainDrone {
    pub fn subscribe_subject(drone: DroneId, cluster: ClusterName) -> SubscribeSubject<Self> {
        SubscribeSubject::new(format!(
            "cluster.{}.drone.{}.drain",
            cluster.subject_name(),
            drone.id()
        ))
    }
}
