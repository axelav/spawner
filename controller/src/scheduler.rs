use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use plane_core::{
    messages::agent::DroneStatusMessage,
    types::{ClusterName, DroneId},
};
use rand::{seq::SliceRandom, thread_rng};
use std::{error::Error, fmt::Display};

#[derive(Default)]
pub struct Scheduler {
    last_status: DashMap<ClusterName, DashMap<DroneId, DateTime<Utc>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SchedulerError {
    NoDroneAvailable,
}

impl Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for SchedulerError {}

impl Scheduler {
    pub fn update_status(&self, timestamp: DateTime<Utc>, status: &DroneStatusMessage) {
        // Drone status is stored in a hashmap for each cluster. There's no external
        // source-of-truth for cluster existence; we simply create a hashmap for a cluster
        // the first time we see a status message for it.
        let cluster_map = self.last_status.entry(status.cluster.clone()).or_default();
        if status.ready {
            // If drone is ready, it gets an entry in cluster hashmap.
            cluster_map.insert(status.drone_id.clone(), timestamp);
        } else {
            // If the drone is not ready, it is removed from the cluster hashmap. If it
            // is not already in this cluster hashmap, this is a no-op.
            cluster_map.remove(&status.drone_id);
        }
    }

    pub fn schedule(
        &self,
        cluster: &ClusterName,
        current_timestamp: DateTime<Utc>,
    ) -> Result<DroneId, SchedulerError> {
        // TODO: this is a dumb placeholder scheduler.

        let threshold_time = current_timestamp
            .checked_sub_signed(Duration::seconds(5))
            .unwrap();

        let cluster_drones = if let Some(cluster_drones) = self.last_status.get(cluster) {
            cluster_drones
        } else {
            tracing::warn!(
                ?cluster,
                "Cluster requested for spawn has never been seen by this controller."
            );
            return Err(SchedulerError::NoDroneAvailable);
        };

        let drone_ids: Vec<DroneId> = cluster_drones
            .iter()
            .filter(|d| d.value() > &threshold_time)
            .map(|d| d.key().clone())
            .collect();

        tracing::info!(
            total_num_candidates=%cluster_drones.len(),
            num_live_candidates=%drone_ids.len(),
            %cluster,
            "Found cluster state to schedule."
        );

        drone_ids
            .choose(&mut thread_rng())
            .cloned()
            .ok_or(SchedulerError::NoDroneAvailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PLANE_VERSION: &str = env!("CARGO_PKG_VERSION");

    fn date(date: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(date).unwrap().into()
    }

    #[test]
    fn test_no_drones() {
        let scheduler = Scheduler::default();
        let timestamp = date("2020-01-01T05:00:00+00:00");
        assert_eq!(
            Err(SchedulerError::NoDroneAvailable),
            scheduler.schedule(&ClusterName::new("mycluster.test"), timestamp)
        );
    }

    #[test]
    fn test_one_drone() {
        let scheduler = Scheduler::default();
        let drone_id = DroneId::new_random();

        scheduler.update_status(
            date("2020-01-01T05:00:00+00:00"),
            &DroneStatusMessage {
                drone_id: drone_id.clone(),
                cluster: ClusterName::new("mycluster.test"),
                drone_version: PLANE_VERSION.to_string(),
                ready: true,
                running_backends: None,
            },
        );

        assert_eq!(
            Ok(drone_id),
            scheduler.schedule(
                &ClusterName::new("mycluster.test"),
                date("2020-01-01T05:00:03+00:00")
            )
        );
    }

    #[test]
    fn test_one_drone_wrong_cluster() {
        let scheduler = Scheduler::default();

        scheduler.update_status(
            date("2020-01-01T05:00:00+00:00"),
            &DroneStatusMessage {
                drone_id: DroneId::new_random(),
                cluster: ClusterName::new("mycluster1.test"),
                drone_version: PLANE_VERSION.to_string(),
                ready: true,
                running_backends: None,
            },
        );

        assert_eq!(
            Err(SchedulerError::NoDroneAvailable),
            scheduler.schedule(
                &ClusterName::new("mycluster2.test"),
                date("2020-01-01T05:00:03+00:00")
            )
        );
    }

    #[test]
    fn test_one_drone_expired() {
        let scheduler = Scheduler::default();

        scheduler.update_status(
            date("2020-01-01T05:00:00+00:00"),
            &DroneStatusMessage {
                drone_id: DroneId::new_random(),
                cluster: ClusterName::new("mycluster.test"),
                drone_version: PLANE_VERSION.to_string(),
                ready: true,
                running_backends: None,
            },
        );

        assert_eq!(
            Err(SchedulerError::NoDroneAvailable),
            scheduler.schedule(
                &ClusterName::new("mycluster.test"),
                date("2020-01-01T05:00:09+00:00")
            )
        );
    }
}
