//! Maps each `Role` to the agent that fulfills it. Built-in agents are registered
//! by the engine; user-defined agents register here too in a later plan.

use std::collections::HashMap;
use std::sync::Arc;

use otto_protocol::Role;

use crate::traits::Agent;

#[derive(Default)]
pub struct AgentRegistry {
    agents: HashMap<Role, Arc<dyn Agent>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }

    pub fn register(&mut self, role: Role, agent: Arc<dyn Agent>) {
        self.agents.insert(role, agent);
    }

    pub fn get(&self, role: &Role) -> anyhow::Result<Arc<dyn Agent>> {
        self.agents
            .get(role)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no agent registered for role {role:?}"))
    }
}
