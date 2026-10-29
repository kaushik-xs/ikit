//! The active org (`ikit org use`) and each org's active workload (`ikit wl use`),
//! stored in `context.yaml` at the ikit home.

use std::collections::BTreeMap;
use std::fs;

use serde::{Deserialize, Serialize};

use crate::{App, AppError, Paths, Result};

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    /// Active workload per org, so switching orgs keeps each org's selection.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workloads: BTreeMap<String, String>,
    /// Pre-org homes stored a single active workload here; read only for migration.
    #[serde(default, skip_serializing)]
    pub workload: Option<String>,
}

impl Context {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.context_file();
        if !path.exists() {
            return Ok(Self::default());
        }
        let yaml = fs::read_to_string(&path)?;
        serde_yaml_ng::from_str(&yaml).map_err(|source| AppError::Yaml { path, source })
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fs::create_dir_all(&paths.home)?;
        let yaml = serde_yaml_ng::to_string(self).expect("context serializes");
        fs::write(paths.context_file(), yaml)?;
        Ok(())
    }
}

/// The org commands act on: `explicit` (from `--org`) or the active one.
pub fn resolve_org(paths: &Paths, explicit: Option<&str>) -> Result<String> {
    match explicit {
        Some(org) => Ok(org.to_owned()),
        None => Context::load(paths)?.org.ok_or(AppError::NoOrgSelected),
    }
}

impl App {
    pub fn current_workload(&self) -> Result<Option<String>> {
        Ok(Context::load(&self.paths)?
            .workloads
            .get(&self.org.name)
            .cloned())
    }

    pub(crate) fn set_current_workload(&self, name: Option<&str>) -> Result<()> {
        let mut ctx = Context::load(&self.paths)?;
        match name {
            Some(n) => ctx.workloads.insert(self.org.name.clone(), n.to_owned()),
            None => ctx.workloads.remove(&self.org.name),
        };
        ctx.save(&self.paths)
    }

    /// `explicit` if given (from `--wl`), otherwise this org's active workload.
    pub fn resolve_workload_name(&self, explicit: Option<&str>) -> Result<String> {
        match explicit {
            Some(name) => Ok(name.to_owned()),
            None => self.current_workload()?.ok_or(AppError::NoWorkloadSelected),
        }
    }
}
