use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use crate::admission::peer::{legacy_owner_for_worker, owner_alive};
use crate::admission::types::AdmissionError;
use crate::platform::SystemInspector;

pub struct LeasePool {
    pub path: PathBuf,
    pub lock_path: PathBuf,
    pub limit_gib: f64,
    pub heavy_builder_isolation: bool,
}

impl LeasePool {
    pub fn new(
        path: PathBuf,
        limit_gib: f64,
        heavy_builder_isolation: bool,
    ) -> Result<Self, AdmissionError> {
        if !limit_gib.is_finite() || limit_gib <= 0.0 {
            return Err(AdmissionError::InvalidBudget(
                "Invalid aggregate memory budget".to_string(),
            ));
        }
        let lock_path = path.with_extension("json.lock");
        Ok(Self {
            path,
            lock_path,
            limit_gib,
            heavy_builder_isolation,
        })
    }

    pub fn acquire(
        &mut self,
        owner: &str,
        amount: f64,
        sys: &dyn SystemInspector,
    ) -> Result<bool, AdmissionError> {
        self.change(owner, Some(amount), None, sys)
    }

    pub fn release(
        &mut self,
        owner: &str,
        sys: &dyn SystemInspector,
    ) -> Result<bool, AdmissionError> {
        self.change(owner, None, None, sys)
    }

    pub fn acquire_runner(
        &mut self,
        owner: &str,
        amount: f64,
        sys: &dyn SystemInspector,
    ) -> Result<bool, AdmissionError> {
        let legacy = legacy_owner_for_worker(owner).ok();
        self.change(owner, Some(amount), legacy.as_deref(), sys)
    }

    pub fn release_runner(
        &mut self,
        owner: &str,
        sys: &dyn SystemInspector,
    ) -> Result<bool, AdmissionError> {
        let legacy = legacy_owner_for_worker(owner).ok();
        self.change(owner, None, legacy.as_deref(), sys)
    }

    pub fn active_leases(&self) -> Result<HashMap<String, f64>, AdmissionError> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let content = fs::read_to_string(&self.path)?;
        let rows = serde_json::from_str(&content).unwrap_or_default();
        Ok(rows)
    }

    fn fits(&self, rows: &HashMap<String, f64>, owner: &str, amount: f64) -> bool {
        let current_sum: f64 = rows
            .iter()
            .filter(|(k, _)| k.as_str() != owner)
            .map(|(_, v)| *v)
            .sum();
        current_sum + amount <= self.limit_gib + 0.0001
    }

    fn change(
        &mut self,
        owner: &str,
        amount: Option<f64>,
        legacy_owner: Option<&str>,
        sys: &dyn SystemInspector,
    ) -> Result<bool, AdmissionError> {
        if let Some(amt) = amount {
            if !amt.is_finite() || amt <= 0.0 || amt > self.limit_gib {
                return Err(AdmissionError::InvalidBudget(
                    "Request exceeds host memory budget".to_string(),
                ));
            }
        }

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Open dedicated lock file with O_CREAT | O_RDWR | O_NOFOLLOW
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&self.lock_path)?;

        let lock_fd = lock_file.as_raw_fd();
        let flock_res = unsafe { libc::flock(lock_fd, libc::LOCK_EX) };
        if flock_res != 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        struct FlockGuard(libc::c_int);
        impl Drop for FlockGuard {
            fn drop(&mut self) {
                unsafe {
                    libc::flock(self.0, libc::LOCK_UN);
                }
            }
        }
        let _guard = FlockGuard(lock_fd);

        let mut rows: HashMap<String, f64> = if self.path.exists() {
            let mut file = OpenOptions::new().read(true).open(&self.path)?;
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        };

        // Prune dead owners
        rows.retain(|key, _| owner_alive(key, sys));

        let legacy_value = legacy_owner.and_then(|lo| rows.remove(lo));
        let mut accepted = true;

        if let Some(amt) = amount {
            if self.heavy_builder_isolation && owner.starts_with("host:") {
                if rows
                    .iter()
                    .any(|(k, v)| k.starts_with("runner:") && (*v - 32.0).abs() < 0.001)
                {
                    accepted = false;
                }
            } else if (amt - 32.0).abs() < 0.001
                && self.heavy_builder_isolation
                && owner.starts_with("runner:")
            {
                if rows.keys().any(|k| k.starts_with("host:")) {
                    accepted = false;
                }
            } else if let Some(legacy_val) = legacy_value {
                if (legacy_val - amt).abs() > 0.001 {
                    accepted = false;
                }
            }

            if accepted {
                if let Some(&existing) = rows.get(owner) {
                    accepted = (existing - amt).abs() < 0.001 && self.fits(&rows, owner, amt);
                } else if self.fits(&rows, owner, amt) {
                    rows.insert(owner.to_string(), amt);
                    accepted = true;
                } else {
                    accepted = false;
                }
            }
        } else {
            // Release
            rows.remove(owner);
            accepted = true;
        }

        if !accepted {
            if let (Some(lo), Some(lv)) = (legacy_owner, legacy_value) {
                rows.insert(lo.to_string(), lv);
            }
        }

        // Commit to .tmp, fsync, and atomic rename
        let tmp_path = self.path.with_extension("tmp");
        {
            let mut tmp_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600)
                .open(&tmp_path)?;

            let tmp_fd = tmp_file.as_raw_fd();
            unsafe {
                libc::fchmod(tmp_fd, 0o600);
            }

            let json_bytes = serde_json::to_vec_pretty(&rows)?;
            tmp_file.write_all(&json_bytes)?;
            tmp_file.flush()?;

            let sync_res = unsafe { libc::fsync(tmp_fd) };
            if sync_res != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }

        fs::rename(&tmp_path, &self.path)?;
        Ok(accepted)
    }
}
