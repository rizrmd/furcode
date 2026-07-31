mod agent;
mod agent_definition;
mod context_engine;
mod conversation;
mod database;
mod furcode_repo;
mod fs_snap;
mod fuzzy_search;
mod provider;
mod skill;
mod validation;

mod proto_generated {
    tonic::include_proto!("forge.v1");
}

// Only expose furcode_repo container
pub use furcode_repo::*;
