//! Any other program: launched unchanged, no hooks. Its activity is `busy` or
//! `quiet` depending on whether it has produced output recently.

use anyhow::Result;
use serde_json::Value;

use super::{Context, Driver, DriverReport, Hint, Launch};

pub struct Generic;

impl Driver for Generic {
    fn kind(&self) -> &'static str {
        "generic"
    }

    fn has_hooks(&self) -> bool {
        false
    }

    fn prepare(&self, _launch: &mut Launch, _ctx: &Context) -> Result<Option<String>> {
        Ok(None)
    }

    fn translate(&self, _event: &Value) -> DriverReport {
        DriverReport::hint(Hint::Ignore)
    }
}
