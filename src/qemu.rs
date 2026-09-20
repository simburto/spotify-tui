use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command};

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows::Win32::System::JobObjects::*;

#[derive(Debug, Clone)]
pub struct QemuConfig {
    pub qemu_binary: PathBuf,
    pub kernel_path: PathBuf,
    pub initrd_path: PathBuf,
    pub disk_image: PathBuf,
    pub memory_mb: u32,
    pub smp_cores: u32,
}

impl Default for QemuConfig {
    fn default() -> Self {
        // Use qemu-system-x86_64w.exe (GUI subsystem) if present, otherwise fallback
        let gui_bin = PathBuf::from("qemu/qemu-system-x86_64w.exe");
        let bin = if gui_bin.exists() {
            gui_bin
        } else {
            PathBuf::from("qemu/qemu-system-x86_64.exe")
        };

        Self {
            qemu_binary: bin,
            kernel_path: PathBuf::from("assets/guest/vmlinuz"),
            initrd_path: PathBuf::from("assets/guest/initrd.img"),
            disk_image: PathBuf::from("assets/guest/rootfs.ext4"),
            memory_mb: 256,
            smp_cores: 1,
        }
    }
}

pub struct QemuBackend {
    config: QemuConfig,
    child: Option<Child>,
    #[cfg(windows)]
    _job: Option<HANDLE>,
}

impl QemuBackend {
    pub fn new(config: QemuConfig) -> Self {
        Self {
            config,
            child: None,
            #[cfg(windows)]
            _job: None,
        }
    }

    pub fn start(&mut self) -> Result<()> {
        if self.child.is_some() {
            log::warn!("QEMU process is already running.");
            return Ok(());
        }

        let qemu_bin = &self.config.qemu_binary;
        if !qemu_bin.exists() {
            anyhow::bail!("QEMU binary not found at {}", qemu_bin.display());
        }
        if !self.config.kernel_path.exists() {
            anyhow::bail!("Kernel not found at {}", self.config.kernel_path.display());
        }
        if !self.config.initrd_path.exists() {
            anyhow::bail!("Initrd not found at {}", self.config.initrd_path.display());
        }
        if !self.config.disk_image.exists() {
            anyhow::bail!("Rootfs not found at {}", self.config.disk_image.display());
        }

        let net_args = [
            "user",
            "id=net0",
            "hostfwd=tcp:127.0.0.1:3000-:3000",
            "hostfwd=tcp:127.0.0.1:4713-:4713",
            "hostfwd=tcp:127.0.0.1:40193-:40193",
            "hostfwd=tcp::5000-:5000",
            "hostfwd=tcp::4714-:4714",
        ]
            .join(",");

        let mut cmd = Command::new(qemu_bin);

        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW (0x0800_0000)
            cmd.creation_flags(0x0800_0000);
        }

        cmd.arg("-L")
            .arg("qemu")
            .arg("-machine")
            .arg("q35,accel=whpx:tcg")
            .arg("-cpu")
            .arg("max,hv-relaxed,hv-vapic,hv-spinlocks=0x1fff,hv-time")
            .arg("-rtc")
            .arg("base=utc,driftfix=slew,clock=vm")
            .arg("-m")
            .arg(format!("{}M", self.config.memory_mb))
            .arg("-smp")
            .arg(self.config.smp_cores.to_string())
            .arg("-nographic")
            .arg("-monitor")
            .arg("none")
            .arg("-serial")
            .arg("null") // Gives Linux a valid UART sink so /bin/sh doesn't stall on I/O
            .arg("-kernel")
            .arg(&self.config.kernel_path)
            .arg("-initrd")
            .arg(&self.config.initrd_path)
            .arg("-append")
            .arg("console=ttyS0 root=/dev/vda rw quiet init=/init nohz=on mitigations=off idle=halt clocksource=tsc")
            .arg("-drive")
            .arg(format!(
                "file={},if=virtio,format=raw",
                self.config.disk_image.display()
            ))
            .arg("-netdev")
            .arg(net_args)
            .arg("-device")
            .arg("virtio-net-pci,netdev=net0");

        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        log::info!("Spawning QEMU headless guest backend...");
        let child = cmd.spawn().context("Failed to spawn QEMU child process")?;

        #[cfg(windows)]
        {
            unsafe {
                if let Ok(job) = CreateJobObjectW(None, None) {
                    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                    let res = SetInformationJobObject(
                        job,
                        JobObjectExtendedLimitInformation,
                        &info as *const _ as *const _,
                        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    );

                    if res.is_ok() {
                        let raw_handle = HANDLE(child.as_raw_handle() as *mut std::ffi::c_void);
                        let _ = AssignProcessToJobObject(job, raw_handle);
                        self._job = Some(job);
                    }
                }
            }
        }

        self.child = Some(child);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            log::info!("Shutting down QEMU process...");
            let _ = child.kill();
            let _ = child.wait();
        }

        #[cfg(windows)]
        if let Some(job) = self._job.take() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(job);
            }
        }
    }
}

impl Drop for QemuBackend {
    fn drop(&mut self) {
        self.stop();
    }
}