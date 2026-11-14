# RamBridge — Use Your Android Phone's RAM as a Virtual SCSI SSD & Pagefile on Windows

Most laptops with 4 GB or 8 GB of soldered RAM choke the moment you open a modern browser, an IDE, and a few background tools at the same time. Meanwhile, the Android phone sitting on your desk has fast LPDDR4X or LPDDR5 RAM sitting mostly idle.

**RamBridge** is a lightweight, low-level experimental system written in **Rust** (Windows Host) and **C++ NDK / Kotlin Compose** (Android) that locks a user-selected chunk of physical RAM on an unrooted Android phone, tunnels it over a standard USB cable, mounts it inside the Windows Kernel as a **Fixed Internal SCSI SSD**, and hot-attaches a Windows `pagefile.sys` to it at runtime without rebooting.

> **Real-World Hardware Test:** I built and tested this setup on a laptop with 4 GB of RAM and my primary daily-driver Samsung phone (`SM-M215F`), which is 6 years old. Windows natively recognizes the phone's pinned RAM buffer in Task Manager as **`ANDROID LPDDR-RAM-SSD SCSI Disk Device` (`Type: SSD`)** with `0 ms` seek latency and zero flash wear on the phone's internal storage.

---

## Why This Shouldn't Work (And How We Bypassed Every OS Restriction)

If you try to do this the normal way, both Android and Windows block you at every layer. Here is how RamBridge works around them from first principles:

| OS Roadblock | Why Standard Methods Fail | How RamBridge Solves It |
| :--- | :--- | :--- |
| **1. Unrooted Android USB** | Modern Android only exposes MTP (Media Transfer) over USB. Windows cannot mount a pagefile on an MTP device, and USB Mass Storage requires Root. | **ADB USB Bulk Socket Tunnel:** Runs a raw binary TCP block server inside native C++ on `127.0.0.1:9999` and bridges it over USB via `adb forward`. |
| **2. Android JVM Heap Limit** | Standard Kotlin/Java Android apps are capped at 256 MB–512 MB of heap memory by the ART runtime. | **Off-Heap Native `mmap()`:** Uses C++ JNI (`MAP_PRIVATE \| MAP_ANONYMOUS`) to request raw memory pages directly from the Android Linux kernel, bypassing the Java heap limit and avoiding the Android Low Memory Killer via lazy page commitment. |
| **3. Windows Blocks USB Pagefiles** | Windows refuses to put `pagefile.sys` on removable USB drives (`RemovableMedia = 1`). | **RFC 3720 iSCSI Target (`127.0.0.1:3260`):** The Rust host speaks the iSCSI protocol directly to Windows' built-in signed `msiscsi.sys` driver, spoofing SCSI Inquiry `RMB = 0` (Non-Removable Fixed Disk) and VPD Page `0xB1` (`0x0001` Non-Rotating Solid State Media). |
| **4. The Reboot Catch-22** | Adding a pagefile in Windows Settings normally requires a reboot—which disconnects the USB tunnel before Windows boots. | **Live NT Kernel Syscall (`NtCreatePagingFile`):** The Rust host acquires `SeCreatePagefilePrivilege` and invokes `ntdll!NtCreatePagingFile` directly to hot-mount `X:\pagefile.sys` live without restarting. |
| **5. USB Cable Disconnect BSOD** | Unplugging an active pagefile drive normally causes a Windows Blue Screen (`KERNEL_DATA_INPAGE_ERROR`). | **Crash-Guard SSD Failover:** Optional toggle that mirrors active blocks to a local fallback file so if the USB cord gets bumped, I/O fails over instead of crashing the kernel. |

---

## Repository Structure

```text
RamBridge-Project/
├── Android-App/                  # Kotlin Jetpack Compose UI + C++17 NDK mmap Block Server
│   └── app/src/main/
│       ├── cpp/native-lib.cpp    # Off-heap mmap allocator & binary socket server
│       └── java/com/rambridge/app/MainActivity.kt
│
└── Windows-Host/                 # Rust eframe/egui GUI + RFC 3720 iSCSI Target + NT Pagefile Injector
    ├── src/main.rs
    └── Cargo.toml
```

---

## How to Use It (Step-by-Step)

### 1. Prepare Your Android Phone
1. Install **`RamBridge-Android.apk`** on your phone (either from the **Releases** tab or by building `Android-App` in Android Studio).
2. Enable **Developer Options** on your phone: go to **Settings -> About Phone -> Software Information** and tap **Build Number** 7 times.
3. Go to **Settings -> Developer Options** and turn ON **USB Debugging**.
4. Plug your phone into your Windows PC with a USB cable.
   * **Important USB Tip:** If turning on USB Debugging alone does not establish the data link, pull down your phone's notification shade, tap **USB Settings**, switch **Use USB for** from *Charging phone only* to **Transferring files / Android Auto**, and then toggle **USB Debugging** on. When the *"Allow USB debugging?"* popup appears on your phone, check **Always allow** and tap **Allow**.

### 2. Lock RAM on the Phone App
1. Open **RamBridge** on your phone. It will display your live Total RAM, Android OS Used RAM, and Free RAM.
2. Choose how much RAM you want to lend (e.g., `512 MB` or `1 GB` for testing, or up to your safe maximum).
3. Flip **Toggle 1 (`Lock Phone RAM Buffer`)** to pin the memory range via native `mmap`.
4. Flip **Toggle 2 (`Emulate Windows Internal SSD`)** to start the USB block server on port `:9999`.

### 3. Mount the Virtual SSD & Pagefile on Windows
1. Launch **`rambridge_host.exe`** on your Windows PC.
2. If you see the amber **NOT RUNNING AS ADMINISTRATOR** banner at the top, click **Relaunch as Administrator (UAC)** (Admin rights are required to control the Windows `MSiSCSI` service and call `NtCreatePagingFile`).
3. Select the **SIZE** button (`512M`, `1024M`, `2048M`, `3072M`) that matches the RAM you locked on your phone, and pick a free **DRIVE** letter (auto-selects `X:` or the next available drive).
4. Check **Toggle 1 (`Bridge Android RAM Server over USB`)**. Click **Run 4KB Live Phone RAM Read/Write Verification** to confirm data is reading and writing to your phone's RAM.
5. Check **Toggle 2 (`Mount Phone RAM as Windows Internal SCSI SSD`)**. Wait ~5–8 seconds while Windows connects to the local iSCSI target, partitions the RAM block, and formats it as **NTFS Drive `X:` (`PhoneRAM_SSD`)**.
   * You can open **Windows Task Manager -> Performance** right now and see **`Disk (X:) ANDROID LPDDR-RAM-SSD SCSI Disk Device`** listed as an **SSD**.
6. Check **Toggle 4 (`Attach Windows Kernel Pagefile`)** to bind `X:\pagefile.sys` live.

---

## Building from Source

### Windows Host (Rust)
Requires Rust (`cargo`) on Windows 10/11:
```powershell
cd Windows-Host
cargo build --release
```
The compiled binary will be generated at `Windows-Host/target/release/rambridge_host.exe`.

### Android App (Kotlin + C++ NDK)
1. Open the `Android-App` folder in **Android Studio** (with the **NDK** and **CMake** installed via SDK Manager).
2. Let Gradle sync, then click **Build -> Generate App Bundles or APKs -> Generate APKs**.

---

## License
MIT License — feel free to fork, experiment, and hack on it.
