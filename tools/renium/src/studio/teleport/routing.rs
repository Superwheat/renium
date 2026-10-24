//! Membership belongs to a client; server lifetime belongs to the root test.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Place {
    pub universe: i64,
    pub place: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Server {
    pub runtime: String,
    pub nonce: String,
    pub pid: u32,
    pub version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ServerState {
    Starting(u64),
    Ready(Server),
}

#[derive(Clone, Debug)]
struct Member {
    place: Place,
    runtime: String,
    generation: u64,
    pending: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) struct Travel {
    pub sequence: u64,
    pub from: Place,
    pub to: Place,
    epoch: u64,
    members: BTreeMap<String, (String, u64)>,
}

pub(super) struct Routes {
    pub servers: BTreeMap<Place, ServerState>,
    source: Place,
    members: BTreeMap<String, Member>,
    epoch: u64,
    sequence: u64,
    stopped: bool,
}

impl Routes {
    pub fn new(source: Place, server: Server) -> Self {
        Self {
            servers: BTreeMap::from([(source, ServerState::Ready(server))]),
            source,
            members: BTreeMap::new(),
            epoch: 0,
            sequence: 0,
            stopped: false,
        }
    }

    pub fn add_client(&mut self, name: String, runtime: String) -> Result<()> {
        ensure!(!self.stopped, "The root Play session ended");
        ensure!(!self.members.contains_key(&name), "Duplicate test client");
        ensure!(self.members.len() < 8, "The test already has eight clients");
        self.members.insert(
            name,
            Member {
                place: self.source,
                runtime,
                generation: 0,
                pending: None,
            },
        );
        Ok(())
    }

    /// Reserve all participants before changing any of them. Independent
    /// groups may prepare concurrently; overlapping groups cannot detach twice.
    pub fn reserve(&mut self, from: Place, to: Place, clients: &[String]) -> Result<Travel> {
        ensure!(!self.stopped, "The root Play session ended");
        let unique: BTreeSet<_> = clients.iter().collect();
        ensure!(
            !clients.is_empty() && unique.len() == clients.len(),
            "Invalid teleport group"
        );
        let mut members = BTreeMap::new();
        for name in clients {
            let member = self
                .members
                .get(name)
                .context("Teleport client left the test")?;
            ensure!(
                member.place == from && member.pending.is_none(),
                "Teleport client moved or is already traveling"
            );
            members.insert(name.clone(), (member.runtime.clone(), member.generation));
        }
        self.sequence += 1;
        for name in clients {
            self.members.get_mut(name).unwrap().pending = Some(self.sequence);
        }
        Ok(Travel {
            sequence: self.sequence,
            from,
            to,
            epoch: self.epoch,
            members,
        })
    }

    pub fn current(&self, travel: &Travel) -> bool {
        !self.stopped
            && self.epoch == travel.epoch
            && travel.members.iter().all(|(name, (runtime, generation))| {
                self.members.get(name).is_some_and(|member| {
                    member.place == travel.from
                        && member.runtime == *runtime
                        && member.generation == *generation
                        && member.pending == Some(travel.sequence)
                })
            })
    }

    /// The caller that inserts Starting owns creation. Other arrivals wait for
    /// this entry, including when their freshly authorized save version differs.
    pub fn start_server(&mut self, travel: &Travel) -> Result<bool> {
        ensure!(self.current(travel), "Stale teleport preparation");
        if self.servers.contains_key(&travel.to) {
            return Ok(false);
        }
        self.servers
            .insert(travel.to, ServerState::Starting(travel.sequence));
        Ok(true)
    }

    pub fn server_ready(&mut self, travel: &Travel, server: Server) -> Result<()> {
        // A participant disconnect does not cancel a server reserved by another
        // arrival. Only the root epoch and the creation ticket own this result.
        ensure!(
            !self.stopped && self.epoch == travel.epoch,
            "The root Play session ended during server creation"
        );
        ensure!(
            self.servers.get(&travel.to) == Some(&ServerState::Starting(travel.sequence)),
            "Destination server creation was replaced"
        );
        self.servers.insert(travel.to, ServerState::Ready(server));
        Ok(())
    }

    pub fn server_failed(&mut self, travel: &Travel) {
        if self.epoch == travel.epoch
            && self.servers.get(&travel.to) == Some(&ServerState::Starting(travel.sequence))
        {
            self.servers.remove(&travel.to);
        }
    }

    pub fn arrive(&mut self, travel: &Travel, runtimes: &BTreeMap<String, String>) -> Result<()> {
        ensure!(
            self.current(travel),
            "Teleport membership changed before arrival"
        );
        ensure!(
            matches!(self.servers.get(&travel.to), Some(ServerState::Ready(_))),
            "Destination server is not ready"
        );
        ensure!(
            runtimes.keys().eq(travel.members.keys())
                && runtimes.values().all(|runtime| !runtime.is_empty()),
            "Destination client group changed"
        );
        for (name, runtime) in runtimes {
            let member = self.members.get_mut(name).unwrap();
            member.place = travel.to;
            member.runtime.clone_from(runtime);
            member.generation += 1;
            member.pending = None;
        }
        Ok(())
    }

    pub fn cancel(&mut self, travel: &Travel) {
        if !self.stopped && self.epoch == travel.epoch {
            for (name, (runtime, generation)) in &travel.members {
                if let Some(member) = self.members.get_mut(name)
                    && member.runtime == *runtime
                    && member.generation == *generation
                    && member.pending == Some(travel.sequence)
                {
                    member.pending = None;
                }
            }
        }
    }

    pub fn disconnect(&mut self, name: &str, runtime: &str) {
        if self
            .members
            .get(name)
            .is_some_and(|member| member.runtime == runtime)
        {
            self.members.remove(name);
        }
    }

    pub fn clients(&self, place: Place) -> BTreeMap<String, String> {
        self.members
            .iter()
            .filter(|(_, member)| member.place == place)
            .map(|(name, member)| (name.clone(), member.runtime.clone()))
            .collect()
    }

    pub fn stop(&mut self) -> BTreeMap<Place, ServerState> {
        self.stopped = true;
        self.epoch += 1;
        self.members.clear();
        std::mem::take(&mut self.servers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: Place = Place {
        universe: 7,
        place: 11,
    };
    const DESTINATION: Place = Place {
        universe: 7,
        place: 12,
    };
    fn server(runtime: &str) -> Server {
        Server {
            runtime: runtime.into(),
            nonce: "root".into(),
            pid: 1,
            version: 3,
        }
    }
    fn routes() -> Routes {
        let mut routes = Routes::new(SOURCE, server("source"));
        routes
            .add_client("Player1".into(), "client-1".into())
            .unwrap();
        routes
            .add_client("Player2".into(), "client-2".into())
            .unwrap();
        routes
    }
    fn reserve(routes: &mut Routes, from: Place, to: Place, player: &str) -> Travel {
        routes.reserve(from, to, &[player.into()]).unwrap()
    }
    fn arrive(routes: &mut Routes, travel: &Travel, player: &str, runtime: &str) {
        routes
            .arrive(travel, &BTreeMap::from([(player.into(), runtime.into())]))
            .unwrap();
    }

    #[test]
    fn simultaneous_first_arrivals_share_one_server_and_leave_others_on_source() {
        let mut routes = routes();
        let first = reserve(&mut routes, SOURCE, DESTINATION, "Player1");
        let second = reserve(&mut routes, SOURCE, DESTINATION, "Player2");
        assert!(routes.start_server(&first).unwrap());
        assert!(!routes.start_server(&second).unwrap());
        routes.server_ready(&first, server("destination")).unwrap();
        arrive(&mut routes, &first, "Player1", "arrived-1");
        assert_eq!(
            routes.clients(SOURCE),
            BTreeMap::from([("Player2".into(), "client-2".into())])
        );
        arrive(&mut routes, &second, "Player2", "arrived-2");
        assert_eq!(routes.servers.len(), 2);
    }

    #[test]
    fn empty_destination_survives_repeated_hops_and_retains_its_saved_version() {
        let mut routes = routes();
        for hop in 0..4 {
            let outbound = reserve(&mut routes, SOURCE, DESTINATION, "Player1");
            assert_eq!(routes.start_server(&outbound).unwrap(), hop == 0);
            if hop == 0 {
                routes
                    .server_ready(&outbound, server("destination"))
                    .unwrap();
            }
            arrive(
                &mut routes,
                &outbound,
                "Player1",
                &format!("destination-{hop}"),
            );
            let inbound = reserve(&mut routes, DESTINATION, SOURCE, "Player1");
            assert!(!routes.start_server(&inbound).unwrap());
            arrive(&mut routes, &inbound, "Player1", &format!("source-{hop}"));
            assert!(routes.clients(DESTINATION).is_empty());
            assert_eq!(
                routes.servers[&DESTINATION],
                ServerState::Ready(server("destination"))
            );
            assert_eq!(routes.clients(SOURCE)["Player2"], "client-2");
        }
        assert_eq!(routes.stop().len(), 2);
    }

    #[test]
    fn stop_fences_pending_starts_arrivals_and_repeated_cleanup() {
        let mut routes = routes();
        let travel = reserve(&mut routes, SOURCE, DESTINATION, "Player1");
        routes.start_server(&travel).unwrap();
        assert_eq!(routes.stop().len(), 2);
        assert!(routes.server_ready(&travel, server("late")).is_err());
        assert!(
            routes
                .arrive(
                    &travel,
                    &BTreeMap::from([("Player1".into(), "late".into())])
                )
                .is_err()
        );
        routes.server_failed(&travel);
        routes.cancel(&travel);
        assert!(routes.servers.is_empty());
        assert!(routes.stop().is_empty());
    }

    #[test]
    fn old_disconnects_and_completions_cannot_change_a_new_membership() {
        let mut routes = routes();
        let travel = reserve(&mut routes, SOURCE, DESTINATION, "Player1");
        routes.start_server(&travel).unwrap();
        routes.server_ready(&travel, server("destination")).unwrap();
        arrive(&mut routes, &travel, "Player1", "new-client");
        routes.disconnect("Player1", "client-1");
        routes.cancel(&travel);
        assert!(
            routes
                .arrive(
                    &travel,
                    &BTreeMap::from([("Player1".into(), "late".into())])
                )
                .is_err()
        );
        assert_eq!(routes.clients(DESTINATION)["Player1"], "new-client");
    }

    #[test]
    fn rejected_group_does_not_reserve_any_of_its_participants() {
        let mut routes = routes();
        assert!(
            routes
                .reserve(SOURCE, DESTINATION, &["Player1".into(), "missing".into()])
                .is_err()
        );
        let travel = reserve(&mut routes, SOURCE, DESTINATION, "Player1");
        assert!(
            routes
                .reserve(SOURCE, DESTINATION, &["Player1".into()])
                .is_err()
        );
        routes.cancel(&travel);
        assert!(
            routes
                .reserve(SOURCE, DESTINATION, &["Player1".into()])
                .is_ok()
        );
    }

    #[test]
    fn disconnected_creator_does_not_discard_another_clients_destination() {
        let mut routes = routes();
        let first = reserve(&mut routes, SOURCE, DESTINATION, "Player1");
        routes.start_server(&first).unwrap();
        let second = reserve(&mut routes, SOURCE, DESTINATION, "Player2");
        routes.disconnect("Player1", "client-1");
        routes.server_ready(&first, server("destination")).unwrap();
        assert!(!routes.start_server(&second).unwrap());
        arrive(&mut routes, &second, "Player2", "destination-2");
    }
}
