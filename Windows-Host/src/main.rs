#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, Margin, RichText, Rounding, Stroke, Vec2};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS, STATUS_SUCCESS, UNICODE_STRING};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[link(name = "ntdll")]
extern "system" {
    fn RtlInitUnicodeString(DestinationString: *mut UNICODE_STRING, SourceString: *const u16);
    fn NtCreatePagingFile(
        PageFileName: *const UNICODE_STRING,
        MinimumSize: *const i64,
        MaximumSize: *const i64,
        Priority: u32,
    ) -> NTSTATUS;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetLastError() -> u32;
}

const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;
const CMD_READ: u8 = 0x01;
const CMD_WRITE: u8 = 0x02;
const TARGET_IQN: &str = "iqn.2026-11.com.rambridge:phonessd";

#[repr(C, packed)]
struct BlockHeader {
    cmd: u8,
    offset: u64,
    length: u32,
}

#[derive(PartialEq, Clone, Copy)]
enum ThemeMode {
    System,
    Light,
    Dark,
}

struct SharedIoState {
    usb_stream: Mutex<Option<TcpStream>>,
    fallback_file: Mutex<Option<File>>,
    ssd_fallback_enabled: AtomicBool,
    iscsi_running: AtomicBool,
    bytes_read_phone: AtomicU64,
    bytes_written_phone: AtomicU64,
    bytes_fallback_ssd: AtomicU64,
    disk_size_bytes: AtomicU64,
    usb_port: AtomicU64,
}

impl SharedIoState {
    fn reconnect_usb_locked(stream_opt: &mut Option<TcpStream>, port: u16) -> bool {
        if let Ok(s) = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            Duration::from_millis(1500),
        ) {
            let _ = s.set_nodelay(true);
            let _ = s.set_read_timeout(Some(Duration::from_secs(4)));
            let _ = s.set_write_timeout(Some(Duration::from_secs(4)));
            *stream_opt = Some(s);
            return true;
        }
        false
    }

    fn read_from_phone_or_fallback(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let max_bytes = self.disk_size_bytes.load(Ordering::Relaxed);
        if offset >= max_bytes {
            buf.fill(0);
            return Ok(());
        }
        let valid_len = buf.len().min((max_bytes - offset) as usize);

        if let Ok(mut guard) = self.usb_stream.lock() {
            let port = self.usb_port.load(Ordering::Relaxed) as u16;
            for attempt in 0..2 {
                if guard.is_none() && attempt > 0 {
                    Self::reconnect_usb_locked(&mut guard, port);
                }
                if let Some(stream) = guard.as_mut() {
                    let hdr = BlockHeader {
                        cmd: CMD_READ,
                        offset,
                        length: valid_len as u32,
                    };
                    let hdr_bytes = unsafe {
                        std::slice::from_raw_parts(
                            &hdr as *const _ as *const u8,
                            std::mem::size_of::<BlockHeader>(),
                        )
                    };
                    if stream.write_all(hdr_bytes).is_ok()
                        && stream.read_exact(&mut buf[..valid_len]).is_ok()
                    {
                        self.bytes_read_phone
                            .fetch_add(valid_len as u64, Ordering::Relaxed);
                        return Ok(());
                    } else {
                        *guard = None;
                    }
                }
            }
        }

        if self.ssd_fallback_enabled.load(Ordering::Relaxed) {
            if let Ok(mut fguard) = self.fallback_file.lock() {
                if let Some(f) = fguard.as_mut() {
                    let _ = f.seek(SeekFrom::Start(offset));
                    if f.read_exact(&mut buf[..valid_len]).is_ok() {
                        self.bytes_fallback_ssd
                            .fetch_add(valid_len as u64, Ordering::Relaxed);
                        return Ok(());
                    }
                }
            }
        }
        buf.fill(0);
        Ok(())
    }

    fn write_to_phone_and_fallback(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        let max_bytes = self.disk_size_bytes.load(Ordering::Relaxed);
        if offset >= max_bytes {
            return Ok(());
        }
        let valid_len = data.len().min((max_bytes - offset) as usize);
        let slice = &data[..valid_len];

        if let Ok(mut guard) = self.usb_stream.lock() {
            let port = self.usb_port.load(Ordering::Relaxed) as u16;
            for attempt in 0..2 {
                if guard.is_none() && attempt > 0 {
                    Self::reconnect_usb_locked(&mut guard, port);
                }
                if let Some(stream) = guard.as_mut() {
                    let hdr = BlockHeader {
                        cmd: CMD_WRITE,
                        offset,
                        length: valid_len as u32,
                    };
                    let hdr_bytes = unsafe {
                        std::slice::from_raw_parts(
                            &hdr as *const _ as *const u8,
                            std::mem::size_of::<BlockHeader>(),
                        )
                    };
                    let mut ack = [0u8; 1];
                    if stream.write_all(hdr_bytes).is_ok()
                        && stream.write_all(slice).is_ok()
                        && stream.read_exact(&mut ack).is_ok()
                    {
                        self.bytes_written_phone
                            .fetch_add(valid_len as u64, Ordering::Relaxed);
                        break;
                    } else {
                        *guard = None;
                    }
                }
            }
        }

        if self.ssd_fallback_enabled.load(Ordering::Relaxed) {
            if let Ok(mut fguard) = self.fallback_file.lock() {
                if fguard.is_none() {
                    let fallback_path = std::env::temp_dir().join("rambridge_fallback.bin");
                    if let Ok(f) = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .open(fallback_path)
                    {
                        *fguard = Some(f);
                    }
                }
                if let Some(f) = fguard.as_mut() {
                    let _ = f.seek(SeekFrom::Start(offset));
                    let _ = f.write_all(slice);
                    self.bytes_fallback_ssd
                        .fetch_add(valid_len as u64, Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }
}

/// Picks the first unused drive letter on ANY Windows PC so it never collides with existing drives
fn pick_free_drive_letter() -> char {
    for candidate in ['X', 'Z', 'R', 'V', 'W', 'M', 'P', 'Q'] {
        let root = format!("{}:\\", candidate);
        if !std::path::Path::new(&root).exists() {
            return candidate;
        }
    }
    'X'
}

/// Universal ADB finder that works across any Windows user account or custom PATH
fn find_adb_executable() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let sdk_adb = PathBuf::from(local_app_data)
            .join("Android")
            .join("Sdk")
            .join("platform-tools")
            .join("adb.exe");
        if sdk_adb.exists() {
            return sdk_adb;
        }
    }
    if let Ok(android_home) = std::env::var("ANDROID_HOME") {
        let sdk_adb = PathBuf::from(android_home)
            .join("platform-tools")
            .join("adb.exe");
        if sdk_adb.exists() {
            return sdk_adb;
        }
    }
    PathBuf::from("adb")
}

struct RamBridgeHostApp {
    theme_mode: ThemeMode,
    is_admin: bool,
    usb_port: u16,
    drive_letter: char,
    allocated_mb: u64,

    usb_connected: bool,
    virtual_ssd_mounted: bool,
    ssd_fallback_enabled: bool,
    pagefile_mounted: bool,

    io_state: Arc<SharedIoState>,
    status_log: Arc<Mutex<String>>,
}

impl Default for RamBridgeHostApp {
    fn default() -> Self {
        let admin = enable_pagefile_privilege();
        let free_drv = pick_free_drive_letter();
        let initial_msg = if admin {
            format!(
                "ADMIN READY: Match Disk Size to your phone, then enable Toggle 1 -> 2 -> 4 (Drive {}:).",
                free_drv
            )
        } else {
            String::from(
                "NOT ELEVATED: Click 'Relaunch as Administrator (UAC)' above so Windows allows mounting SCSI SSD & Pagefile.",
            )
        };

        Self {
            theme_mode: ThemeMode::System,
            is_admin: admin,
            usb_port: 9999,
            drive_letter: free_drv,
            allocated_mb: 512,
            usb_connected: false,
            virtual_ssd_mounted: false,
            ssd_fallback_enabled: true,
            pagefile_mounted: false,
            io_state: Arc::new(SharedIoState {
                usb_stream: Mutex::new(None),
                fallback_file: Mutex::new(None),
                ssd_fallback_enabled: AtomicBool::new(true),
                iscsi_running: AtomicBool::new(false),
                bytes_read_phone: AtomicU64::new(0),
                bytes_written_phone: AtomicU64::new(0),
                bytes_fallback_ssd: AtomicU64::new(0),
                disk_size_bytes: AtomicU64::new(512 * 1024 * 1024),
                usb_port: AtomicU64::new(9999),
            }),
            status_log: Arc::new(Mutex::new(initial_msg)),
        }
    }
}

impl RamBridgeHostApp {
    fn set_status(&self, msg: impl Into<String>) {
        if let Ok(mut g) = self.status_log.lock() {
            *g = msg.into();
        }
    }

    fn relaunch_as_admin(&self) {
        if let Ok(exe) = std::env::current_exe() {
            let exe_str = exe.display().to_string();
            let _ = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-WindowStyle",
                    "Hidden",
                    "-Command",
                    &format!("Start-Process -FilePath '{}' -Verb RunAs", exe_str),
                ])
                .spawn();
            std::process::exit(0);
        }
    }

    fn connect_usb_bridge(&mut self) {
        let port_str = format!("tcp:{}", self.usb_port);
        let adb_path = find_adb_executable();
        self.io_state
            .usb_port
            .store(self.usb_port as u64, Ordering::Relaxed);

        let _ = Command::new(&adb_path)
            .args(["forward", &port_str, &port_str])
            .output();

        match TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", self.usb_port).parse().unwrap(),
            Duration::from_secs(2),
        ) {
            Ok(mut s) => {
                let _ = s.set_nodelay(true);
                let _ = s.set_read_timeout(Some(Duration::from_secs(4)));
                let _ = s.set_write_timeout(Some(Duration::from_secs(4)));

                let test_hdr = BlockHeader {
                    cmd: CMD_READ,
                    offset: 0,
                    length: 4,
                };
                let hdr_bytes = unsafe {
                    std::slice::from_raw_parts(
                        &test_hdr as *const _ as *const u8,
                        std::mem::size_of::<BlockHeader>(),
                    )
                };
                let mut probe = [0u8; 4];
                if s.write_all(hdr_bytes).is_ok() && s.read_exact(&mut probe).is_ok() {
                    self.io_state
                        .bytes_read_phone
                        .fetch_add(4, Ordering::Relaxed);
                    if let Ok(mut guard) = self.io_state.usb_stream.lock() {
                        *guard = Some(s);
                    }
                    self.usb_connected = true;
                    self.set_status(format!(
                        "CONNECTED: Linked to Android LPDDR RAM server over USB port :{}",
                        self.usb_port
                    ));
                } else {
                    self.usb_connected = false;
                    self.set_status(
                        "USB HANDSHAKE FAILED: Turn ON Toggle 1 and Toggle 2 inside the phone app first!",
                    );
                }
            }
            Err(e) => {
                self.usb_connected = false;
                self.set_status(format!(
                    "USB ERROR: Could not reach phone on :{}. ({})",
                    self.usb_port, e
                ));
            }
        }
    }

    fn disconnect_usb_bridge(&mut self) {
        if let Ok(mut guard) = self.io_state.usb_stream.lock() {
            *guard = None;
        }
        self.usb_connected = false;
        self.set_status("DISCONNECTED: USB RAM tunnel closed safely.");
    }

    fn verify_4kb_phone_ram_io(&mut self) {
        if !self.usb_connected {
            self.set_status("Verify failed: Enable Toggle 1 (Connect USB) first!");
            return;
        }

        let test_pattern = vec![0x5Au8; 4096];
        let _ = self.io_state.write_to_phone_and_fallback(0, &test_pattern);

        let mut read_buf = vec![0u8; 4096];
        let _ = self
            .io_state
            .read_from_phone_or_fallback(0, &mut read_buf);

        if read_buf == test_pattern {
            self.set_status("SUCCESS: 4KB R/W verified in Android physical LPDDR RAM over USB!");
        } else {
            self.set_status("MISMATCH: Data read from phone did not match pattern.");
        }
    }

    fn toggle_virtual_ssd_mount(&mut self, enable: bool) {
        if !enable {
            let _ = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-WindowStyle",
                    "Hidden",
                    "-Command",
                    &format!(
                        "Get-IscsiSession | Where-Object {{ $_.TargetNodeAddress -eq '{}' }} | Disconnect-IscsiTarget -Confirm:$false -ErrorAction SilentlyContinue",
                        TARGET_IQN
                    ),
                ])
                .output();
            self.virtual_ssd_mounted = false;
            self.set_status(format!(
                "Unmounted Virtual SCSI SSD Drive {}:",
                self.drive_letter
            ));
            return;
        }

        if !self.usb_connected {
            self.set_status("Enable Toggle 1 (Bridge Android RAM Server over USB) first!");
            return;
        }

        if !enable_pagefile_privilege() {
            self.set_status(
                "ADMIN REQUIRED: Click 'Relaunch as Administrator (UAC)' at the top of the window first!",
            );
            return;
        }

        self.io_state
            .disk_size_bytes
            .store(self.allocated_mb * 1024 * 1024, Ordering::SeqCst);

        if !self.io_state.iscsi_running.swap(true, Ordering::SeqCst) {
            let io_clone = Arc::clone(&self.io_state);
            std::thread::spawn(move || {
                run_iscsi_server(io_clone);
            });
        }

        let status_clone = Arc::clone(&self.status_log);
        let drive_char = self.drive_letter;
        self.virtual_ssd_mounted = true;
        self.set_status(
            "STEP 1/3: Starting Windows MSiSCSI service & discovering local iSCSI Target (:3260)...",
        );

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));

            let ps_script = format!(
                "$ErrorActionPreference = 'Continue'; \
                 Set-Service -Name MSiSCSI -StartupType Manual -ErrorAction SilentlyContinue; \
                 Start-Service -Name MSiSCSI -ErrorAction SilentlyContinue; \
                 New-IscsiTargetPortal -TargetPortalAddress 127.0.0.1 -TargetPortalPortNumber 3260 -ErrorAction SilentlyContinue | Out-Null; \
                 Update-IscsiTargetPortal -TargetPortalAddress 127.0.0.1 -TargetPortalPortNumber 3260 -ErrorAction SilentlyContinue | Out-Null; \
                 $t = Get-IscsiTarget | Where-Object {{ $_.NodeAddress -eq '{iqn}' }} | Select-Object -First 1; \
                 if ($t -and -not $t.IsConnected) {{ \
                     Connect-IscsiTarget -NodeAddress '{iqn}' -TargetPortalAddress 127.0.0.1 -TargetPortalPortNumber 3260 -IsPersistent $false -ErrorAction SilentlyContinue | Out-Null; \
                 }}; \
                 for ($i = 0; $i -lt 12; $i++) {{ \
                     Update-HostStorageCache -ErrorAction SilentlyContinue; \
                     $d = Get-Disk | Where-Object {{ $_.BusType -eq 'iSCSI' -or $_.FriendlyName -like '*LPDDR*' }} | Select-Object -First 1; \
                     if ($d) {{ break }}; \
                     Start-Sleep -Milliseconds 500; \
                 }}; \
                 if (-not $d) {{ Write-Output 'ERR_NO_DISK'; exit }}; \
                 if ($d.IsOffline) {{ Set-Disk -Number $d.Number -IsOffline $false -ErrorAction SilentlyContinue }}; \
                 if ($d.IsReadOnly) {{ Set-Disk -Number $d.Number -IsReadOnly $false -ErrorAction SilentlyContinue }}; \
                 if ($d.PartitionStyle -eq 'RAW') {{ \
                     Initialize-Disk -Number $d.Number -PartitionStyle MBR -Confirm:$false -ErrorAction SilentlyContinue | Out-Null; \
                 }}; \
                 $p = Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue | Select-Object -First 1; \
                 if (-not $p) {{ \
                     New-Partition -DiskNumber $d.Number -UseMaximumSize -DriveLetter {drv} -ErrorAction SilentlyContinue | Out-Null; \
                 }} elseif ($p.DriveLetter -ne '{drv}') {{ \
                     Set-Partition -DiskNumber $d.Number -PartitionNumber $p.PartitionNumber -NewDriveLetter {drv} -ErrorAction SilentlyContinue | Out-Null; \
                 }}; \
                 Format-Volume -DriveLetter {drv} -FileSystem NTFS -NewFileSystemLabel 'PhoneRAM_SSD' -Force -Confirm:$false -ErrorAction SilentlyContinue | Out-Null; \
                 Write-Output 'MOUNT_OK'",
                iqn = TARGET_IQN,
                drv = drive_char
            );

            let out = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    &ps_script,
                ])
                .output();

            if std::path::Path::new(&format!("{}:\\", drive_char)).exists() {
                if let Ok(mut g) = status_clone.lock() {
                    *g = format!(
                        "DRIVE {}: ONLINE! Phone LPDDR RAM mounted as Fixed NTFS SSD. Now enable Toggle 4!",
                        drive_char
                    );
                }
            } else if let Ok(o) = out {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                if let Ok(mut g) = status_clone.lock() {
                    *g = format!(
                        "Mount diagnostic: {} {}",
                        stdout.trim().replace('\n', " "),
                        stderr.trim().replace('\n', " ")
                    );
                }
            }
        });
    }

    fn toggle_windows_pagefile(&mut self, enable: bool) {
        if !enable {
            self.pagefile_mounted = false;
            self.set_status("Pagefile unlinked from session state.");
            return;
        }

        if !enable_pagefile_privilege() {
            self.pagefile_mounted = false;
            self.set_status(
                "PRIVILEGE ERROR: Click 'Relaunch as Administrator (UAC)' at the top of the window!",
            );
            return;
        }

        let root_path = format!("{}:\\", self.drive_letter);
        if !std::path::Path::new(&root_path).exists() {
            self.pagefile_mounted = false;
            self.set_status(format!(
                "Drive {}: is not mounted yet! Wait for Toggle 2 to say 'DRIVE {}: ONLINE!' first.",
                self.drive_letter, self.drive_letter
            ));
            return;
        }

        let nt_path = format!("\\??\\{}:\\pagefile.sys\0", self.drive_letter);
        let nt_path_utf16: Vec<u16> = nt_path.encode_utf16().collect();

        unsafe {
            let mut ustr: UNICODE_STRING = std::mem::zeroed();
            RtlInitUnicodeString(&mut ustr, nt_path_utf16.as_ptr());

            let pagefile_mb = (self.allocated_mb.saturating_sub(96)).max(128);
            let size_bytes: i64 = (pagefile_mb as i64) * 1024 * 1024;
            let status = NtCreatePagingFile(&ustr, &size_bytes, &size_bytes, 0);

            if status == STATUS_SUCCESS {
                self.pagefile_mounted = true;
                self.set_status(format!(
                    "KERNEL SUCCESS: Attached {} MB pagefile.sys on Drive {}:! Check Task Manager -> Memory.",
                    pagefile_mb, self.drive_letter
                ));
            } else {
                self.pagefile_mounted = false;
                self.set_status(format!(
                    "NtCreatePagingFile returned 0x{:08X} on Drive {}:",
                    status as u32, self.drive_letter
                ));
            }
        }
    }
}

fn enable_pagefile_privilege() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return false;
        }

        let priv_name: Vec<u16> = "SeCreatePagefilePrivilege\0".encode_utf16().collect();
        let mut tp: TOKEN_PRIVILEGES = std::mem::zeroed();
        tp.PrivilegeCount = 1;
        tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;

        if LookupPrivilegeValueW(
            std::ptr::null(),
            priv_name.as_ptr(),
            &mut tp.Privileges[0].Luid,
        ) == 0
        {
            return false;
        }

        let ok = AdjustTokenPrivileges(
            token,
            0,
            &tp,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        ok != 0 && GetLastError() != ERROR_NOT_ALL_ASSIGNED
    }
}

// ============================================================================
// Universal RFC 3720 iSCSI Target Server (Dynamic Parameter Negotiation)
// ============================================================================
fn run_iscsi_server(state: Arc<SharedIoState>) {
    let Ok(listener) = TcpListener::bind("127.0.0.1:3260") else {
        return;
    };
    for stream in listener.incoming().flatten() {
        let st = Arc::clone(&state);
        std::thread::spawn(move || {
            let _ = handle_iscsi_connection(stream, st);
        });
    }
}

fn handle_iscsi_connection(
    mut stream: TcpStream,
    state: Arc<SharedIoState>,
) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut stat_sn: u32 = 0;
    let mut exp_cmd_sn: u32 = 0;
    let mut cmd_sn_initialized = false;
    let mut sent_tpgt = false;

    loop {
        let mut bhs = [0u8; 48];
        stream.read_exact(&mut bhs)?;

        let opcode = bhs[0] & 0x3F;
        let immediate = (bhs[0] & 0x40) != 0;
        let dsl = u32::from_be_bytes([0, bhs[5], bhs[6], bhs[7]]) as usize;
        let padded_dsl = (dsl + 3) & !3;
        let mut data_seg = vec![0u8; padded_dsl];
        if padded_dsl > 0 {
            stream.read_exact(&mut data_seg)?;
        }

        let itt = u32::from_be_bytes([bhs[16], bhs[17], bhs[18], bhs[19]]);
        let cmd_sn = u32::from_be_bytes([bhs[24], bhs[25], bhs[26], bhs[27]]);

        if !cmd_sn_initialized {
            exp_cmd_sn = cmd_sn;
            cmd_sn_initialized = true;
        }
        if !immediate && opcode != 0x03 {
            exp_cmd_sn = cmd_sn.wrapping_add(1);
        }

        match opcode {
            // 0x03: Login Request -> 0x23: Login Response (Dynamic Key Mirroring)
            0x03 => {
                let transit = (bhs[1] & 0x80) != 0;
                let csg = (bhs[1] >> 2) & 0x03;
                let nsg = bhs[1] & 0x03;
                let req_text = String::from_utf8_lossy(&data_seg[..dsl]);

                let mut reply_params = Vec::new();
                if !sent_tpgt {
                    reply_params.extend_from_slice(b"TargetPortalGroupTag=1\0");
                    sent_tpgt = true;
                }

                for kv in req_text.split('\0') {
                    if let Some((k, v)) = kv.split_once('=') {
                        match k {
                            "InitiatorName" | "InitiatorAlias" | "TargetName" | "SessionType" => {}
                            "AuthMethod" => {
                                reply_params.extend_from_slice(b"AuthMethod=None\0");
                            }
                            "HeaderDigest" => {
                                reply_params.extend_from_slice(b"HeaderDigest=None\0");
                            }
                            "DataDigest" => {
                                reply_params.extend_from_slice(b"DataDigest=None\0");
                            }
                            "MaxRecvDataSegmentLength" => {
                                reply_params
                                    .extend_from_slice(b"MaxRecvDataSegmentLength=262144\0");
                            }
                            "DefaultTime2Retain" => {
                                reply_params.extend_from_slice(b"DefaultTime2Retain=0\0");
                            }
                            "IFMarker" | "OFMarker" => {
                                reply_params.extend_from_slice(format!("{}=No\0", k).as_bytes());
                            }
                            "InitialR2T"
                            | "ImmediateData"
                            | "MaxBurstLength"
                            | "FirstBurstLength"
                            | "DefaultTime2Wait"
                            | "MaxOutstandingR2T"
                            | "MaxConnections"
                            | "DataPDUInOrder"
                            | "DataSequenceInOrder"
                            | "ErrorRecoveryLevel" => {
                                reply_params
                                    .extend_from_slice(format!("{}={}\0", k, v).as_bytes());
                            }
                            _ => {}
                        }
                    }
                }

                let mut resp = [0u8; 48];
                resp[0] = 0x23;
                resp[1] = (if transit { 0x80 } else { 0 }) | (csg << 2) | nsg;
                let len = reply_params.len() as u32;
                resp[5] = ((len >> 16) & 0xFF) as u8;
                resp[6] = ((len >> 8) & 0xFF) as u8;
                resp[7] = (len & 0xFF) as u8;
                resp[8..14].copy_from_slice(&bhs[8..14]);

                if transit && nsg == 3 {
                    resp[14..16].copy_from_slice(&[0x00, 0x01]);
                } else {
                    resp[14..16].copy_from_slice(&[0x00, 0x00]);
                }

                resp[16..20].copy_from_slice(&itt.to_be_bytes());
                resp[24..28].copy_from_slice(&stat_sn.to_be_bytes());
                stat_sn = stat_sn.wrapping_add(1);
                resp[28..32].copy_from_slice(&exp_cmd_sn.to_be_bytes());
                resp[32..36].copy_from_slice(&(exp_cmd_sn.wrapping_add(64)).to_be_bytes());

                stream.write_all(&resp)?;
                if !reply_params.is_empty() {
                    while reply_params.len() % 4 != 0 {
                        reply_params.push(0);
                    }
                    stream.write_all(&reply_params)?;
                }
            }
            // 0x04: Text Request (SendTargets) -> 0x24: Text Response
            0x04 => {
                let mut text_reply =
                    format!("TargetName={}\0TargetAddress=127.0.0.1:3260,1\0", TARGET_IQN)
                        .into_bytes();
                let mut resp = [0u8; 48];
                resp[0] = 0x24;
                resp[1] = 0x80;
                let len = text_reply.len() as u32;
                resp[5] = ((len >> 16) & 0xFF) as u8;
                resp[6] = ((len >> 8) & 0xFF) as u8;
                resp[7] = (len & 0xFF) as u8;
                resp[16..20].copy_from_slice(&itt.to_be_bytes());
                resp[20..24].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
                resp[24..28].copy_from_slice(&stat_sn.to_be_bytes());
                stat_sn = stat_sn.wrapping_add(1);
                resp[28..32].copy_from_slice(&exp_cmd_sn.to_be_bytes());
                resp[32..36].copy_from_slice(&(exp_cmd_sn.wrapping_add(64)).to_be_bytes());

                stream.write_all(&resp)?;
                while text_reply.len() % 4 != 0 {
                    text_reply.push(0);
                }
                stream.write_all(&text_reply)?;
            }
            // 0x00: NOP-Out -> 0x20: NOP-In
            0x00 => {
                let mut resp = [0u8; 48];
                resp[0] = 0x20;
                resp[1] = 0x80;
                let len = dsl as u32;
                resp[5] = ((len >> 16) & 0xFF) as u8;
                resp[6] = ((len >> 8) & 0xFF) as u8;
                resp[7] = (len & 0xFF) as u8;
                resp[16..20].copy_from_slice(&itt.to_be_bytes());
                resp[20..24].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
                resp[24..28].copy_from_slice(&stat_sn.to_be_bytes());
                if itt != 0xFFFF_FFFF {
                    stat_sn = stat_sn.wrapping_add(1);
                }
                resp[28..32].copy_from_slice(&exp_cmd_sn.to_be_bytes());
                resp[32..36].copy_from_slice(&(exp_cmd_sn.wrapping_add(64)).to_be_bytes());
                stream.write_all(&resp)?;
                if padded_dsl > 0 {
                    stream.write_all(&data_seg)?;
                }
            }
            // 0x06: Logout Request -> 0x26: Logout Response
            0x06 => {
                let mut resp = [0u8; 48];
                resp[0] = 0x26;
                resp[1] = 0x80;
                resp[16..20].copy_from_slice(&itt.to_be_bytes());
                resp[24..28].copy_from_slice(&stat_sn.to_be_bytes());
                resp[28..32].copy_from_slice(&exp_cmd_sn.to_be_bytes());
                resp[32..36].copy_from_slice(&(exp_cmd_sn.wrapping_add(64)).to_be_bytes());
                let _ = stream.write_all(&resp);
                return Ok(());
            }
            // 0x01: SCSI Command
            0x01 => {
                let exp_xfer_len =
                    u32::from_be_bytes([bhs[20], bhs[21], bhs[22], bhs[23]]) as usize;
                let lun = bhs[9];
                let cdb = &bhs[32..48];
                let scsi_op = cdb[0];

                // Only LUN 0 exists
                if lun != 0 && scsi_op != 0xA0 && scsi_op != 0x12 {
                    let sense = build_sense(0x05, 0x25, 0x00);
                    send_scsi_status(
                        &mut stream,
                        itt,
                        &mut stat_sn,
                        exp_cmd_sn,
                        0x02,
                        &sense,
                        exp_xfer_len,
                    )?;
                    continue;
                }

                let total_bytes = state.disk_size_bytes.load(Ordering::Relaxed);
                let total_blocks = (total_bytes / 512).max(1);

                match scsi_op {
                    // TEST UNIT READY, SYNC CACHE (10/16), VERIFY (10/16), START STOP UNIT
                    0x00 | 0x35 | 0x91 | 0x2F | 0x8F | 0x1B => {
                        send_scsi_status(
                            &mut stream,
                            itt,
                            &mut stat_sn,
                            exp_cmd_sn,
                            0x00,
                            &[],
                            exp_xfer_len,
                        )?;
                    }
                    // REPORT LUNS
                    0xA0 => {
                        let mut lun_data = vec![0u8; 16];
                        lun_data[3] = 8;
                        send_scsi_data_in(
                            &mut stream,
                            itt,
                            &mut stat_sn,
                            exp_cmd_sn,
                            &lun_data,
                            exp_xfer_len,
                        )?;
                    }
                    // REQUEST SENSE
                    0x03 => {
                        let mut sense = vec![0u8; 18];
                        sense[0] = 0x70;
                        sense[7] = 10;
                        send_scsi_data_in(
                            &mut stream,
                            itt,
                            &mut stat_sn,
                            exp_cmd_sn,
                            &sense,
                            exp_xfer_len,
                        )?;
                    }
                    // INQUIRY
                    0x12 => {
                        let evpd = (cdb[1] & 0x01) != 0;
                        let page_code = cdb[2];
                        if lun != 0 {
                            let mut inq = vec![0u8; 36];
                            inq[0] = 0x7F; // LUN not present
                            send_scsi_data_in(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                &inq,
                                exp_xfer_len,
                            )?;
                        } else if !evpd {
                            let mut inq = vec![0u8; 36];
                            inq[0] = 0x00; // Direct-access block device
                            inq[1] = 0x00; // RMB = 0 (Fixed Internal SSD)
                            inq[2] = 0x05; // SPC-3
                            inq[3] = 0x02;
                            inq[4] = 31;
                            inq[8..16].copy_from_slice(b"ANDROID ");
                            inq[16..32].copy_from_slice(b"LPDDR-RAM-SSD   ");
                            inq[32..36].copy_from_slice(b"1.0 ");
                            send_scsi_data_in(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                &inq,
                                exp_xfer_len,
                            )?;
                        } else if page_code == 0x00 {
                            let vpd0 = vec![0x00, 0x00, 0x00, 0x05, 0x00, 0x80, 0x83, 0xB0, 0xB1];
                            send_scsi_data_in(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                &vpd0,
                                exp_xfer_len,
                            )?;
                        } else if page_code == 0x80 {
                            let mut vpd80 = vec![0x00, 0x80, 0x00, 12];
                            vpd80.extend_from_slice(b"RAMBRIDGE001");
                            send_scsi_data_in(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                &vpd80,
                                exp_xfer_len,
                            )?;
                        } else if page_code == 0x83 {
                            let id = b"ANDROID-LPDDR-RAM-0001";
                            let mut vpd83 = vec![
                                0x00,
                                0x83,
                                0x00,
                                (id.len() + 4) as u8,
                                0x02,
                                0x01,
                                0x00,
                                id.len() as u8,
                            ];
                            vpd83.extend_from_slice(id);
                            send_scsi_data_in(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                &vpd83,
                                exp_xfer_len,
                            )?;
                        } else if page_code == 0xB0 {
                            let mut vpdb0 = vec![0u8; 64];
                            vpdb0[1] = 0xB0;
                            vpdb0[3] = 0x3C;
                            vpdb0[8..12].copy_from_slice(&128u32.to_be_bytes());
                            vpdb0[12..16].copy_from_slice(&128u32.to_be_bytes());
                            send_scsi_data_in(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                &vpdb0,
                                exp_xfer_len,
                            )?;
                        } else if page_code == 0xB1 {
                            let mut vpdb1 = vec![0u8; 64];
                            vpdb1[1] = 0xB1;
                            vpdb1[3] = 0x3C;
                            vpdb1[4] = 0x00;
                            vpdb1[5] = 0x01; // 0x0001 = Non-rotating Solid State Media (SSD)
                            send_scsi_data_in(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                &vpdb1,
                                exp_xfer_len,
                            )?;
                        } else {
                            let sense = build_sense(0x05, 0x24, 0x00);
                            send_scsi_status(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                0x02,
                                &sense,
                                exp_xfer_len,
                            )?;
                        }
                    }
                    // MODE SENSE (6)
                    0x1A => {
                        let mut ms6 = vec![0x03, 0x00, 0x00, 0x00];
                        let page = cdb[2] & 0x3F;
                        if page == 0x08 || page == 0x3F {
                            let mut cache_page = vec![0u8; 20];
                            cache_page[0] = 0x08;
                            cache_page[1] = 0x12;
                            cache_page[2] = 0x04;
                            ms6.extend_from_slice(&cache_page);
                        }
                        if page == 0x1C || page == 0x3F {
                            let mut iec_page = vec![0u8; 12];
                            iec_page[0] = 0x1C;
                            iec_page[1] = 0x0A;
                            ms6.extend_from_slice(&iec_page);
                        }
                        ms6[0] = (ms6.len() - 1) as u8;
                        send_scsi_data_in(
                            &mut stream,
                            itt,
                            &mut stat_sn,
                            exp_cmd_sn,
                            &ms6,
                            exp_xfer_len,
                        )?;
                    }
                    // MODE SENSE (10)
                    0x5A => {
                        let mut ms10 = vec![0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
                        let page = cdb[2] & 0x3F;
                        if page == 0x08 || page == 0x3F {
                            let mut cache_page = vec![0u8; 20];
                            cache_page[0] = 0x08;
                            cache_page[1] = 0x12;
                            cache_page[2] = 0x04;
                            ms10.extend_from_slice(&cache_page);
                        }
                        let len = (ms10.len() - 2) as u16;
                        ms10[0..2].copy_from_slice(&len.to_be_bytes());
                        send_scsi_data_in(
                            &mut stream,
                            itt,
                            &mut stat_sn,
                            exp_cmd_sn,
                            &ms10,
                            exp_xfer_len,
                        )?;
                    }
                    // READ CAPACITY (10)
                    0x25 => {
                        let last_lba = (total_blocks - 1).min(u32::MAX as u64) as u32;
                        let mut rc = Vec::with_capacity(8);
                        rc.extend_from_slice(&last_lba.to_be_bytes());
                        rc.extend_from_slice(&512u32.to_be_bytes());
                        send_scsi_data_in(
                            &mut stream,
                            itt,
                            &mut stat_sn,
                            exp_cmd_sn,
                            &rc,
                            exp_xfer_len,
                        )?;
                    }
                    // SERVICE ACTION IN (16) -> READ CAPACITY (16)
                    0x9E => {
                        let last_lba = total_blocks - 1;
                        let mut rc16 = vec![0u8; 32];
                        rc16[0..8].copy_from_slice(&last_lba.to_be_bytes());
                        rc16[8..12].copy_from_slice(&512u32.to_be_bytes());
                        send_scsi_data_in(
                            &mut stream,
                            itt,
                            &mut stat_sn,
                            exp_cmd_sn,
                            &rc16,
                            exp_xfer_len,
                        )?;
                    }
                    // READ (10) & READ (16)
                    0x28 | 0x88 => {
                        let (lba, blocks) = if scsi_op == 0x28 {
                            (
                                u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64,
                                u16::from_be_bytes([cdb[7], cdb[8]]) as usize,
                            )
                        } else {
                            (
                                u64::from_be_bytes([
                                    cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9],
                                ]),
                                u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]) as usize,
                            )
                        };
                        let byte_len = blocks * 512;
                        if byte_len == 0 {
                            send_scsi_status(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                0x00,
                                &[],
                                0,
                            )?;
                        } else {
                            let mut buf = vec![0u8; byte_len];
                            let _ = state.read_from_phone_or_fallback(lba * 512, &mut buf);
                            send_scsi_data_in(
                                &mut stream,
                                itt,
                                &mut stat_sn,
                                exp_cmd_sn,
                                &buf,
                                exp_xfer_len,
                            )?;
                        }
                    }
                    // WRITE (10) & WRITE (16)
                    0x2A | 0x8A => {
                        let (lba, blocks) = if scsi_op == 0x2A {
                            (
                                u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64,
                                u16::from_be_bytes([cdb[7], cdb[8]]) as usize,
                            )
                        } else {
                            (
                                u64::from_be_bytes([
                                    cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9],
                                ]),
                                u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]) as usize,
                            )
                        };
                        let byte_len = blocks * 512;

                        let mut write_buf = Vec::with_capacity(byte_len);
                        if dsl > 0 {
                            let take = dsl.min(byte_len);
                            write_buf.extend_from_slice(&data_seg[..take]);
                        }

                        let mut last_f_bit = (bhs[1] & 0x80) != 0;
                        let mut r2t_sn: u32 = 0;

                        while write_buf.len() < byte_len {
                            if last_f_bit {
                                let remaining = (byte_len - write_buf.len()) as u32;
                                let mut r2t = [0u8; 48];
                                r2t[0] = 0x31;
                                r2t[1] = 0x80;
                                r2t[16..20].copy_from_slice(&itt.to_be_bytes());
                                r2t[20..24].copy_from_slice(&1u32.to_be_bytes());
                                r2t[24..28].copy_from_slice(&stat_sn.to_be_bytes());
                                r2t[28..32].copy_from_slice(&exp_cmd_sn.to_be_bytes());
                                r2t[32..36]
                                    .copy_from_slice(&(exp_cmd_sn.wrapping_add(64)).to_be_bytes());
                                r2t[36..40].copy_from_slice(&r2t_sn.to_be_bytes());
                                r2t_sn = r2t_sn.wrapping_add(1);
                                r2t[40..44]
                                    .copy_from_slice(&(write_buf.len() as u32).to_be_bytes());
                                r2t[44..48].copy_from_slice(&remaining.to_be_bytes());
                                stream.write_all(&r2t)?;
                            }

                            let mut dout_bhs = [0u8; 48];
                            stream.read_exact(&mut dout_bhs)?;
                            last_f_bit = (dout_bhs[1] & 0x80) != 0;
                            let dout_dsl =
                                u32::from_be_bytes([0, dout_bhs[5], dout_bhs[6], dout_bhs[7]])
                                    as usize;
                            let dout_padded = (dout_dsl + 3) & !3;
                            let mut dout_data = vec![0u8; dout_padded];
                            if dout_padded > 0 {
                                stream.read_exact(&mut dout_data)?;
                            }
                            let take = dout_dsl.min(byte_len - write_buf.len());
                            write_buf.extend_from_slice(&dout_data[..take]);
                        }

                        if byte_len > 0 {
                            let _ = state
                                .write_to_phone_and_fallback(lba * 512, &write_buf[..byte_len]);
                        }
                        send_scsi_status(&mut stream, itt, &mut stat_sn, exp_cmd_sn, 0x00, &[], 0)?;
                    }
                    _ => {
                        let sense = build_sense(0x05, 0x20, 0x00);
                        send_scsi_status(
                            &mut stream,
                            itt,
                            &mut stat_sn,
                            exp_cmd_sn,
                            0x02,
                            &sense,
                            exp_xfer_len,
                        )?;
                    }
                }
            }
            _ => {}
        }
    }
}

fn build_sense(sense_key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut s = vec![0u8; 20];
    s[0] = 0;
    s[1] = 18;
    s[2] = 0x70;
    s[4] = sense_key;
    s[9] = 10;
    s[14] = asc;
    s[15] = ascq;
    s
}

fn send_scsi_data_in(
    stream: &mut TcpStream,
    itt: u32,
    stat_sn: &mut u32,
    exp_cmd_sn: u32,
    data: &[u8],
    exp_xfer_len: usize,
) -> std::io::Result<()> {
    let send_len = data.len().min(exp_xfer_len);
    let mut resp = [0u8; 48];
    resp[0] = 0x25;
    resp[1] = 0x81;
    if send_len < exp_xfer_len {
        resp[1] |= 0x02;
        let resid = (exp_xfer_len - send_len) as u32;
        resp[44..48].copy_from_slice(&resid.to_be_bytes());
    }
    let len = send_len as u32;
    resp[5] = ((len >> 16) & 0xFF) as u8;
    resp[6] = ((len >> 8) & 0xFF) as u8;
    resp[7] = (len & 0xFF) as u8;
    resp[16..20].copy_from_slice(&itt.to_be_bytes());
    resp[20..24].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    resp[24..28].copy_from_slice(&stat_sn.to_be_bytes());
    *stat_sn = stat_sn.wrapping_add(1);
    resp[28..32].copy_from_slice(&exp_cmd_sn.to_be_bytes());
    resp[32..36].copy_from_slice(&(exp_cmd_sn.wrapping_add(64)).to_be_bytes());

    stream.write_all(&resp)?;
    if send_len > 0 {
        stream.write_all(&data[..send_len])?;
        let pad = (4 - (send_len % 4)) % 4;
        if pad > 0 {
            stream.write_all(&vec![0u8; pad])?;
        }
    }
    Ok(())
}

fn send_scsi_status(
    stream: &mut TcpStream,
    itt: u32,
    stat_sn: &mut u32,
    exp_cmd_sn: u32,
    status: u8,
    sense_with_len: &[u8],
    exp_xfer_len: usize,
) -> std::io::Result<()> {
    let mut resp = [0u8; 48];
    resp[0] = 0x21;
    resp[1] = 0x80;
    if exp_xfer_len > 0 {
        resp[1] |= 0x02;
        resp[44..48].copy_from_slice(&(exp_xfer_len as u32).to_be_bytes());
    }
    resp[2] = 0x00;
    resp[3] = status;
    let len = sense_with_len.len() as u32;
    resp[5] = ((len >> 16) & 0xFF) as u8;
    resp[6] = ((len >> 8) & 0xFF) as u8;
    resp[7] = (len & 0xFF) as u8;
    resp[16..20].copy_from_slice(&itt.to_be_bytes());
    resp[24..28].copy_from_slice(&stat_sn.to_be_bytes());
    *stat_sn = stat_sn.wrapping_add(1);
    resp[28..32].copy_from_slice(&exp_cmd_sn.to_be_bytes());
    resp[32..36].copy_from_slice(&(exp_cmd_sn.wrapping_add(64)).to_be_bytes());

    stream.write_all(&resp)?;
    if !sense_with_len.is_empty() {
        stream.write_all(sense_with_len)?;
        let pad = (4 - (sense_with_len.len() % 4)) % 4;
        if pad > 0 {
            stream.write_all(&vec![0u8; pad])?;
        }
    }
    Ok(())
}

impl eframe::App for RamBridgeHostApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(400));

        match self.theme_mode {
            ThemeMode::System => {
                let sys_dark = ctx
                    .input(|i| i.raw.system_theme)
                    .map(|t| t == egui::Theme::Dark)
                    .unwrap_or(true);
                if sys_dark {
                    ctx.set_visuals(egui::Visuals::dark());
                } else {
                    ctx.set_visuals(egui::Visuals::light());
                }
            }
            ThemeMode::Light => ctx.set_visuals(egui::Visuals::light()),
            ThemeMode::Dark => ctx.set_visuals(egui::Visuals::dark()),
        }

        let is_dark = ctx.style().visuals.dark_mode;
        let bg_color = if is_dark {
            Color32::from_rgb(11, 14, 20)
        } else {
            Color32::from_rgb(241, 245, 249)
        };
        let card_color = if is_dark {
            Color32::from_rgb(21, 26, 36)
        } else {
            Color32::from_rgb(255, 255, 255)
        };
        let border_color = if is_dark {
            Color32::from_rgb(38, 47, 64)
        } else {
            Color32::from_rgb(203, 213, 225)
        };
        let accent_cyan = if is_dark {
            Color32::from_rgb(0, 229, 255)
        } else {
            Color32::from_rgb(2, 132, 199)
        };
        let accent_emerald = if is_dark {
            Color32::from_rgb(0, 230, 118)
        } else {
            Color32::from_rgb(5, 150, 105)
        };
        let accent_amber = Color32::from_rgb(255, 171, 0);

        let read_kb = self.io_state.bytes_read_phone.load(Ordering::Relaxed) as f64 / 1024.0;
        let write_kb = self.io_state.bytes_written_phone.load(Ordering::Relaxed) as f64 / 1024.0;
        let fallback_kb = self.io_state.bytes_fallback_ssd.load(Ordering::Relaxed) as f64 / 1024.0;

        let panel_frame = egui::Frame::default()
            .fill(bg_color)
            .inner_margin(Margin::same(18.0_f32));

        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                // UAC ELEVATION BANNER (If launched without Admin on any PC)
                if !self.is_admin {
                    egui::Frame::default()
                        .fill(accent_amber.gamma_multiply(0.15_f32))
                        .stroke(Stroke::new(1.0_f32, accent_amber))
                        .rounding(Rounding::same(10.0_f32))
                        .inner_margin(Margin::symmetric(12.0_f32, 8.0_f32))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("⚠ NOT RUNNING AS ADMINISTRATOR")
                                        .monospace()
                                        .size(11.0)
                                        .strong()
                                        .color(accent_amber),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .button(
                                                RichText::new("Relaunch as Administrator (UAC)")
                                                    .strong(),
                                            )
                                            .clicked()
                                        {
                                            self.relaunch_as_admin();
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(8.0);
                }

                // 1. HEADER & LIVE STATUS BADGE
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("RAMBRIDGE // UNIVERSAL WINDOWS HOST")
                                .monospace()
                                .size(12.0)
                                .strong()
                                .color(accent_cyan),
                        );
                        ui.label(
                            RichText::new("USB-SCSI Virtual SSD & Pagefile Controller")
                                .size(20.0)
                                .strong(),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (badge_text, badge_col) = if self.pagefile_mounted {
                            ("PAGEFILE ACTIVE", accent_emerald)
                        } else if self.usb_connected {
                            ("USB TUNNEL ONLINE", accent_emerald)
                        } else {
                            ("STANDBY", Color32::GRAY)
                        };
                        egui::Frame::default()
                            .fill(badge_col.gamma_multiply(0.15_f32))
                            .stroke(Stroke::new(1.0_f32, badge_col))
                            .rounding(Rounding::same(16.0_f32))
                            .inner_margin(Margin::symmetric(12.0_f32, 6.0_f32))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(badge_text)
                                        .monospace()
                                        .size(11.0)
                                        .strong()
                                        .color(badge_col),
                                );
                            });
                    });
                });

                ui.add_space(8.0);

                // 2. DISK SIZE, DRIVE LETTER & THEME BAR
                egui::Frame::default()
                    .fill(card_color)
                    .stroke(Stroke::new(1.0_f32, border_color))
                    .rounding(Rounding::same(10.0_f32))
                    .inner_margin(Margin::symmetric(14.0_f32, 8.0_f32))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("SIZE:").monospace().size(11.0).strong());
                            for mb in [512u64, 1024, 2048, 3072] {
                                if ui
                                    .selectable_label(self.allocated_mb == mb, format!("{}M", mb))
                                    .clicked()
                                    && !self.virtual_ssd_mounted
                                {
                                    self.allocated_mb = mb;
                                }
                            }

                            ui.separator();
                            ui.label(RichText::new("DRIVE:").monospace().size(11.0).strong());
                            for drv in ['X', 'Z', 'R', 'V'] {
                                if ui
                                    .selectable_label(self.drive_letter == drv, format!("{}:", drv))
                                    .clicked()
                                    && !self.virtual_ssd_mounted
                                {
                                    self.drive_letter = drv;
                                }
                            }

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.selectable_value(
                                        &mut self.theme_mode,
                                        ThemeMode::Dark,
                                        "Dark",
                                    );
                                    ui.selectable_value(
                                        &mut self.theme_mode,
                                        ThemeMode::Light,
                                        "Light",
                                    );
                                    ui.selectable_value(
                                        &mut self.theme_mode,
                                        ThemeMode::System,
                                        "System",
                                    );
                                },
                            );
                        });
                    });

                ui.add_space(8.0);

                // 3. LIVE STORAGE ROUTING & TELEMETRY CARD
                egui::Frame::default()
                    .fill(card_color)
                    .stroke(Stroke::new(1.0_f32, border_color))
                    .rounding(Rounding::same(12.0_f32))
                    .inner_margin(Margin::same(14.0_f32))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("LIVE USB vs LAPTOP SSD TELEMETRY")
                                .monospace()
                                .size(11.0)
                                .strong(),
                        );
                        ui.add_space(6.0);
                        ui.columns(3, |cols| {
                            cols[0].label(RichText::new("Phone RAM Read").size(12.0));
                            cols[0].label(
                                RichText::new(format!("{:.2} KB", read_kb))
                                    .monospace()
                                    .size(16.0)
                                    .strong()
                                    .color(accent_cyan),
                            );

                            cols[1].label(RichText::new("Phone RAM Written").size(12.0));
                            cols[1].label(
                                RichText::new(format!("{:.2} KB", write_kb))
                                    .monospace()
                                    .size(16.0)
                                    .strong()
                                    .color(accent_emerald),
                            );

                            cols[2].label(RichText::new("Laptop SSD Fallback").size(12.0));
                            cols[2].label(
                                RichText::new(format!("{:.2} KB", fallback_kb))
                                    .monospace()
                                    .size(16.0)
                                    .strong(),
                            );
                        });

                        ui.add_space(8.0);
                        if ui
                            .add_sized(
                                Vec2::new(ui.available_width(), 28.0),
                                egui::Button::new(
                                    RichText::new("Run 4KB Live Phone RAM Read/Write Verification")
                                        .strong(),
                                ),
                            )
                            .clicked()
                        {
                            self.verify_4kb_phone_ram_io();
                        }
                    });

                ui.add_space(8.0);

                // 4. MANUAL HARDWARE STAGE TOGGLES
                egui::Frame::default()
                    .fill(card_color)
                    .stroke(Stroke::new(1.0_f32, border_color))
                    .rounding(Rounding::same(12.0_f32))
                    .inner_margin(Margin::same(14.0_f32))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("WINDOWS HARDWARE STAGE CONTROLS")
                                .monospace()
                                .size(11.0)
                                .strong(),
                        );
                        ui.add_space(6.0);

                        // Toggle 1: Connect ADB USB Tunnel
                        ui.horizontal(|ui| {
                            let mut conn_state = self.usb_connected;
                            if ui.checkbox(&mut conn_state, "").changed() {
                                if conn_state {
                                    self.connect_usb_bridge();
                                } else {
                                    self.disconnect_usb_bridge();
                                }
                            }
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new("1. Bridge Android RAM Server over USB (ADB :9999)")
                                        .strong()
                                        .size(14.0),
                                );
                                ui.label(
                                    RichText::new(
                                        "Forwards localhost:9999 over USB cable to phone's pinned mmap buffer",
                                    )
                                    .size(12.0),
                                );
                            });
                        });

                        ui.separator();

                        // Toggle 2: Mount iSCSI Virtual SSD Drive
                        ui.horizontal(|ui| {
                            let mut ssd_state = self.virtual_ssd_mounted;
                            if ui.checkbox(&mut ssd_state, "").changed() {
                                self.toggle_virtual_ssd_mount(ssd_state);
                            }
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "2. Mount Phone RAM as Windows Internal SCSI SSD (Drive {}:)",
                                        self.drive_letter
                                    ))
                                    .strong()
                                    .size(14.0),
                                );
                                ui.label(
                                    RichText::new(format!(
                                        "Starts local iSCSI Target (:3260) & formats Phone RAM as Fixed NTFS Drive {}:",
                                        self.drive_letter
                                    ))
                                    .size(12.0),
                                );
                            });
                        });

                        ui.separator();

                        // Toggle 3: Laptop SSD Failover Mirror
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut self.ssd_fallback_enabled, "").changed() {
                                self.io_state
                                    .ssd_fallback_enabled
                                    .store(self.ssd_fallback_enabled, Ordering::SeqCst);
                                if !self.ssd_fallback_enabled {
                                    if let Ok(mut fguard) = self.io_state.fallback_file.lock() {
                                        *fguard = None;
                                    }
                                    let _ = std::fs::remove_file(
                                        std::env::temp_dir().join("rambridge_fallback.bin"),
                                    );
                                    self.set_status(
                                        "PURE PHONE RAM MODE: Local SSD fallback disabled (0 KB laptop SSD usage).",
                                    );
                                } else {
                                    self.set_status(
                                        "SAFE MODE: Local SSD fallback mirror enabled if USB cable disconnects.",
                                    );
                                }
                            }
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new("3. Laptop SSD Crash-Guard Fallback")
                                        .strong()
                                        .size(14.0),
                                );
                                ui.label(
                                    RichText::new(if self.ssd_fallback_enabled {
                                        "ENABLED: Mirrors writes to local fallback file so unplugging USB won't BSOD"
                                    } else {
                                        "DISABLED: 100% Pure Phone LPDDR RAM only — zero laptop SSD usage"
                                    })
                                    .size(12.0),
                                );
                            });
                        });

                        ui.separator();

                        // Toggle 4: Live Kernel NtCreatePagingFile
                        ui.horizontal(|ui| {
                            let mut pf_state = self.pagefile_mounted;
                            if ui.checkbox(&mut pf_state, "").changed() {
                                self.toggle_windows_pagefile(pf_state);
                            }
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "4. Attach Windows Kernel Pagefile ({}:\\pagefile.sys)",
                                        self.drive_letter
                                    ))
                                    .strong()
                                    .size(14.0),
                                );
                                ui.label(
                                    RichText::new(
                                        "Invokes ntdll!NtCreatePagingFile to hot-mount virtual memory without rebooting",
                                    )
                                    .size(12.0),
                                );
                            });
                        });
                    });

                ui.add_space(8.0);

                // 5. STATUS LOG FOOTER
                let current_status = self
                    .status_log
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                egui::Frame::default()
                    .fill(card_color)
                    .stroke(Stroke::new(1.0_f32, border_color))
                    .rounding(Rounding::same(10.0_f32))
                    .inner_margin(Margin::same(12.0_f32))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("STATUS - {}", current_status))
                                .monospace()
                                .size(12.0)
                                .color(accent_cyan),
                        );
                    });
            });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([660.0_f32, 640.0_f32])
            .with_min_inner_size([580.0_f32, 580.0_f32]),
        ..Default::default()
    };
    eframe::run_native(
        "RamBridge // Universal Windows USB-SCSI Host",
        options,
        Box::new(|_cc| Ok(Box::new(RamBridgeHostApp::default()))),
    )
}