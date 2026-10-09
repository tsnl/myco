use serde::{Deserialize, Serialize};

use crate::data::Principal;

/// The incarnation changes whenever the external resource is recreated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Control {
    incarnation: String,
    epoch: u64,
    holder: Option<Principal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    incarnation: String,
    epoch: u64,
    holder: Principal,
}

impl Control {
    pub fn new(incarnation: &str) -> Self {
        Self {
            incarnation: incarnation.into(),
            epoch: 0,
            holder: None,
        }
    }

    /// Called by the resource arbiter after its transfer policy authorizes the change.
    /// A grant carries evidence of authority; it is not an authentication credential.
    pub fn transfer(&self, holder: Option<Principal>) -> Self {
        Self {
            incarnation: self.incarnation.clone(),
            epoch: self.epoch + 1,
            holder,
        }
    }

    pub fn grant(&self, caller: &Principal) -> Result<Grant, &'static str> {
        if self.holder.as_ref() != Some(caller) {
            return Err("caller does not hold control");
        }
        Ok(Grant {
            incarnation: self.incarnation.clone(),
            epoch: self.epoch,
            holder: caller.clone(),
        })
    }

    /// Check and the beginning of a mutation must be serialized by the resource owner.
    pub fn check(&self, caller: &Principal, grant: &Grant) -> Result<(), &'static str> {
        if self.incarnation != grant.incarnation
            || self.epoch != grant.epoch
            || &grant.holder != caller
            || self.holder.as_ref() != Some(caller)
        {
            return Err("control grant is stale or belongs to another caller");
        }
        Ok(())
    }
}
