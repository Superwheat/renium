pub(crate) mod assets;
pub(crate) mod command;
mod discovery;
pub(crate) mod keys;
mod paging;
mod parameters;
pub(crate) mod products;
mod routes;
mod servers;
mod transport;

pub(crate) use discovery::{
    place_history_page, place_versions, team_create_members, version_publish_status,
};
pub(crate) use transport::{
    API_ROOT, CloudAuth, CloudIdentity, agent, execute_one, execute_with_identity, introspect_key,
    read_response,
};
