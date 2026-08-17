use anyhow::{bail, Result};

use crate::model::SystemSample;

pub trait SystemProbe {
    fn sample(&self) -> Result<SystemSample>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealSystemProbe;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
impl SystemProbe for RealSystemProbe {
    fn sample(&self) -> Result<SystemSample> {
        linux::sample()
    }
}

#[cfg(not(target_os = "linux"))]
impl SystemProbe for RealSystemProbe {
    fn sample(&self) -> Result<SystemSample> {
        bail!("the real continuity probe is supported only on Linux")
    }
}
