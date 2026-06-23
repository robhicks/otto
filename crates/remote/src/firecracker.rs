//! `FirecrackerProvisioner` — boots an ephemeral microVM running `otto serve --accept-promotions`,
//! returns a `ProvisionedMachine` whose guardian task kills the VM on drop. Behind the `firecracker`
//! cargo feature: default builds never compile it. The actual boot needs an operator-supplied kernel
//! + rootfs and a host hypervisor, so it cannot run in CI; the pure builders below are unit-tested.

use std::process::Child;
use std::time::Instant;

use async_trait::async_trait;

use crate::{MicrovmConfig, ProvisionedMachine, Provisioner};

/// Build the Firecracker machine-config JSON (consumed via `--config-file`): a boot-source (kernel +
/// guest cmdline), a single root drive (the rootfs), one network interface bound to the host tap, and
/// the machine sizing. `boot_args` carries the guest contract (`otto.token`/`otto.port`/`otto.root`).
fn fc_config_json(config: &MicrovmConfig, token: &str) -> serde_json::Value {
    serde_json::json!({
        "boot-source": {
            "kernel_image_path": config.kernel.to_string_lossy(),
            "boot_args": guest_cmdline(config, token),
        },
        "drives": [{
            "drive_id": "rootfs",
            "path_on_host": config.rootfs.to_string_lossy(),
            "is_root_device": true,
            "is_read_only": false,
        }],
        "network-interfaces": [{
            "iface_id": "eth0",
            "host_dev_name": config.tap,
            "guest_mac": "AA:FC:00:00:00:01",
        }],
        "machine-config": {
            "vcpu_count": config.vcpus,
            "mem_size_mib": config.mem_mib,
        },
    })
}

/// The guest kernel cmdline: a minimal console plus the otto contract the rootfs init reads to launch
/// `otto serve --accept-promotions`. The token rides the cmdline (single-tenant ephemeral guest; same
/// trust domain as the source — `/proc/cmdline` exposure inside the guest is acceptable in v1).
fn guest_cmdline(config: &MicrovmConfig, token: &str) -> String {
    format!(
        "console=ttyS0 reboot=k panic=1 pci=off \
         otto.token={token} otto.port={port} otto.root=/workspace",
        port = config.port,
    )
}

/// The URL the host polls to detect the guest serve is up. Every real route is gated, so any HTTP
/// response (401/404 included) means "serve is listening".
fn readiness_url(config: &MicrovmConfig) -> String {
    format!("http://{}:{}/", config.guest_ip, config.port)
}

/// Fail fast if the operator-supplied prerequisites are missing, before spawning anything.
fn validate_prereqs(config: &MicrovmConfig) -> anyhow::Result<()> {
    for (label, path) in [
        ("firecracker binary", &config.fc_bin),
        ("kernel image", &config.kernel),
        ("rootfs image", &config.rootfs),
    ] {
        if !path.exists() {
            anyhow::bail!("microVM prerequisite missing: {label} not found at {}", path.display());
        }
    }
    Ok(())
}

/// Owns the running VM + its scratch dir; `Drop` (sync) kills the child and removes the dir, so
/// aborting the guardian task disposes the machine. It does not delete the tap (operator-created).
struct FirecrackerGuard {
    child: Child,
    jail_dir: std::path::PathBuf,
}

impl Drop for FirecrackerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.jail_dir);
    }
}

/// Boots ephemeral Firecracker microVMs. `token` is the bearer the guest serve requires (shared from
/// the source, as loopback/vps do).
pub struct FirecrackerProvisioner {
    config: MicrovmConfig,
    token: String,
}

impl FirecrackerProvisioner {
    pub fn new(config: MicrovmConfig, token: impl Into<String>) -> Self {
        Self { config, token: token.into() }
    }
}

#[async_trait]
impl Provisioner for FirecrackerProvisioner {
    async fn provision(&self) -> anyhow::Result<ProvisionedMachine> {
        validate_prereqs(&self.config)?;

        // Per-machine scratch dir + config file.
        let jail_dir = std::env::temp_dir().join(format!("otto-fc-{}", self.config.guest_ip));
        std::fs::create_dir_all(&jail_dir)?;
        let cfg_path = jail_dir.join("vm-config.json");
        let cfg = fc_config_json(&self.config, &self.token);
        std::fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg)?)?;

        // Spawn firecracker with the config file.
        let child = std::process::Command::new(&self.config.fc_bin)
            .arg("--no-api")
            .arg("--config-file")
            .arg(&cfg_path)
            .current_dir(&jail_dir)
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn firecracker: {e}"))?;

        let guard = FirecrackerGuard { child, jail_dir };

        // Poll until the guest serve answers (any HTTP status) or the boot timeout elapses.
        let url = readiness_url(&self.config);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()?;
        let deadline = Instant::now() + self.config.boot_timeout;
        loop {
            if client.get(&url).send().await.is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                // `guard` drops here → VM killed, scratch removed. Nothing leaks on timeout.
                anyhow::bail!("microVM did not become reachable within boot timeout");
            }
            // No tokio `time` feature available; yield and re-poll. The 2s reqwest timeout paces the
            // loop so this is not a busy-spin in the common (still-booting) case.
            tokio::task::yield_now().await;
        }

        // Guardian task: owns the guard, parks until aborted; abort → guard Drop → VM disposed.
        let task = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });

        Ok(ProvisionedMachine {
            endpoint: format!("ws://{}:{}", self.config.guest_ip, self.config.port),
            token: self.token.clone(),
            task,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_config() -> MicrovmConfig {
        MicrovmConfig {
            kernel: PathBuf::from("/img/vmlinux"),
            rootfs: PathBuf::from("/img/rootfs.ext4"),
            fc_bin: PathBuf::from("/usr/bin/firecracker"),
            tap: "fc-tap0".to_string(),
            guest_ip: "172.16.0.2".to_string(),
            port: 7878,
            vcpus: 2,
            mem_mib: 1024,
            boot_timeout: std::time::Duration::from_secs(10),
        }
    }

    #[test]
    fn config_json_has_boot_drive_network_and_machine() {
        let v = fc_config_json(&sample_config(), "tok");
        assert_eq!(v["boot-source"]["kernel_image_path"], "/img/vmlinux");
        assert_eq!(v["drives"][0]["is_root_device"], true);
        assert_eq!(v["drives"][0]["path_on_host"], "/img/rootfs.ext4");
        assert_eq!(v["network-interfaces"][0]["host_dev_name"], "fc-tap0");
        assert_eq!(v["machine-config"]["vcpu_count"], 2);
        assert_eq!(v["machine-config"]["mem_size_mib"], 1024);
    }

    #[test]
    fn guest_cmdline_carries_the_otto_contract() {
        let line = guest_cmdline(&sample_config(), "secret-tok");
        assert!(line.contains("otto.token=secret-tok"), "{line}");
        assert!(line.contains("otto.port=7878"), "{line}");
        assert!(line.contains("otto.root=/workspace"), "{line}");
    }

    #[test]
    fn readiness_url_is_guest_ip_and_port() {
        assert_eq!(readiness_url(&sample_config()), "http://172.16.0.2:7878/");
    }

    #[test]
    fn validate_prereqs_errors_when_a_path_is_missing() {
        let err = validate_prereqs(&sample_config()).unwrap_err().to_string();
        assert!(err.contains("firecracker binary"), "{err}");
    }

    #[test]
    fn validate_prereqs_ok_when_all_present() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            p
        };
        let config = MicrovmConfig {
            fc_bin: mk("firecracker"),
            kernel: mk("vmlinux"),
            rootfs: mk("rootfs.ext4"),
            ..sample_config()
        };
        assert!(validate_prereqs(&config).is_ok());
    }
}
