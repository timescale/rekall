use std::collections::HashMap;

use crate::error::Result;

use super::{InfraOutput, InfraStatus};

#[allow(dead_code)]
pub trait InfraBackend {
    fn name(&self) -> &str;
    fn up(
        &self,
        ssh_pub_key: &str,
        ssh_priv_path: &str,
        ssh_cidr: &str,
        deploy_key_paths: &[String],
        vars: &serde_json::Value,
        user_data: &str,
        instance_name: Option<&str>,
    ) -> Result<InfraOutput>;
    fn down(&self) -> Result<()>;
    fn status(&self) -> Result<InfraStatus>;
    fn outputs(&self) -> Result<HashMap<String, String>>;
    fn has_state(&self) -> bool;
}
