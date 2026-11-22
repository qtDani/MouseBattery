//! Minimal Windows HID access: find Logitech HID++ interfaces, open them, read/write reports.

use std::collections::BTreeMap;
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::HumanInterfaceDevice::*;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::*;
use windows_sys::Win32::System::Threading::*;

const LOGITECH_VID: u16 = 0x046D;

/// A pair of HID++ collections (short = report 0x10, long = report 0x11) of one USB device.
#[derive(Debug, Clone)]
pub struct HidppPaths {
    pub short: String,
    pub long: String,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Lists every Logitech device exposing both HID++ collections (usage page 0xFF00, usage 1 and 2).
pub fn enumerate() -> Vec<HidppPaths> {
    let mut groups: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    unsafe {
        let mut guid: GUID = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(&guid, null(), null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set == INVALID_HANDLE_VALUE as HDEVINFO {
            return Vec::new();
        }
        let mut i = 0u32;
        loop {
            let mut did: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            did.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, null(), &guid, i, &mut did) == 0 {
                break;
            }
            i += 1;

            let mut needed = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &did, null_mut(), 0, &mut needed, null_mut());
            if needed == 0 {
                continue;
            }
            // u64 buffer keeps the struct correctly aligned
            let mut buf = vec![0u64; (needed as usize + 7) / 8 + 1];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &did, detail, needed, null_mut(), null_mut()) == 0 {
                continue;
            }
            let p = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            let path = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            let lower = path.to_ascii_lowercase();
            if !lower.contains("vid_046d") {
                continue;
            }
            let Some((usage_page, usage)) = query_usage(&path) else { continue };
            if usage_page != 0xFF00 || (usage != 1 && usage != 2) {
                continue;
            }
            // Group the two collections of the same interface: "hid#vid_046d&pid_c094&mi_02&col01#..." -> "vid_046d&pid_c094&mi_02"
            let key = lower
                .split('#')
                .nth(1)
                .unwrap_or("")
                .split('&')
                .filter(|part| !part.starts_with("col"))
                .collect::<Vec<_>>()
                .join("&");
            let entry = groups.entry(key).or_default();
            if usage == 1 {
                entry.0 = Some(path);
            } else {
                entry.1 = Some(path);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    groups
        .into_values()
        .filter_map(|(s, l)| Some(HidppPaths { short: s?, long: l? }))
        .collect()
}

fn query_usage(path: &str) -> Option<(u16, u16)> {
    unsafe {
        let w = wide(path);
        // Access 0: lets us read attributes even when another app holds the device open
        let h = CreateFileW(w.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, 0, null_mut());
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut attrs: HIDD_ATTRIBUTES = std::mem::zeroed();
        attrs.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
        let mut result = None;
        if HidD_GetAttributes(h, &mut attrs) != 0 && attrs.VendorID == LOGITECH_VID {
            let mut pp: PHIDP_PREPARSED_DATA = 0;
            if HidD_GetPreparsedData(h, &mut pp) != 0 {
                let mut caps: HIDP_CAPS = std::mem::zeroed();
                if HidP_GetCaps(pp, &mut caps) == HIDP_STATUS_SUCCESS {
                    result = Some((caps.UsagePage, caps.Usage));
                }
                HidD_FreePreparsedData(pp);
            }
        }
        CloseHandle(h);
        result
    }
}

/// An open HID collection using overlapped I/O, so reads and writes never block each other.
pub struct HidDevice {
    handle: HANDLE,
    out_len: usize,
    in_len: usize,
}

unsafe impl Send for HidDevice {}
unsafe impl Sync for HidDevice {}

pub enum ReadResult {
    Data(Vec<u8>),
    Timeout,
    Gone,
}

impl HidDevice {
    pub fn open(path: &str) -> Option<HidDevice> {
        unsafe {
            let w = wide(path);
            let h = CreateFileW(
                w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut out_len = 0usize;
            let mut in_len = 0usize;
            let mut pp: PHIDP_PREPARSED_DATA = 0;
            if HidD_GetPreparsedData(h, &mut pp) != 0 {
                let mut caps: HIDP_CAPS = std::mem::zeroed();
                if HidP_GetCaps(pp, &mut caps) == HIDP_STATUS_SUCCESS {
                    out_len = caps.OutputReportByteLength as usize;
                    in_len = caps.InputReportByteLength as usize;
                }
                HidD_FreePreparsedData(pp);
            }
            if in_len == 0 {
                CloseHandle(h);
                return None;
            }
            Some(HidDevice { handle: h, out_len, in_len })
        }
    }

    pub fn write(&self, data: &[u8]) -> bool {
        let len = self.out_len.max(data.len());
        let mut buf = vec![0u8; len];
        buf[..data.len()].copy_from_slice(data);
        unsafe {
            let event = CreateEventW(null(), 1, 0, null());
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = event;
            let mut written = 0u32;
            let mut ok = WriteFile(self.handle, buf.as_ptr(), len as u32, &mut written, &mut ov) != 0;
            if !ok && GetLastError() == ERROR_IO_PENDING {
                if WaitForSingleObject(event, 1000) == WAIT_OBJECT_0 {
                    ok = GetOverlappedResult(self.handle, &ov, &mut written, 0) != 0;
                } else {
                    CancelIoEx(self.handle, &ov);
                    GetOverlappedResult(self.handle, &ov, &mut written, 1);
                    ok = false;
                }
            }
            CloseHandle(event);
            ok
        }
    }

    pub fn read(&self, timeout_ms: u32) -> ReadResult {
        let mut buf = vec![0u8; self.in_len];
        unsafe {
            let event = CreateEventW(null(), 1, 0, null());
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = event;
            let mut read = 0u32;
            let result;
            if ReadFile(self.handle, buf.as_mut_ptr(), self.in_len as u32, &mut read, &mut ov) != 0 {
                result = ReadResult::Data(buf[..read as usize].to_vec());
            } else if GetLastError() != ERROR_IO_PENDING {
                result = ReadResult::Gone;
            } else if WaitForSingleObject(event, timeout_ms) == WAIT_OBJECT_0 {
                result = if GetOverlappedResult(self.handle, &ov, &mut read, 0) != 0 {
                    ReadResult::Data(buf[..read as usize].to_vec())
                } else {
                    ReadResult::Gone
                };
            } else {
                CancelIoEx(self.handle, &ov);
                // Wait for the cancel to finish so the buffer is no longer in use
                if GetOverlappedResult(self.handle, &ov, &mut read, 1) != 0 && read > 0 {
                    result = ReadResult::Data(buf[..read as usize].to_vec());
                } else {
                    result = ReadResult::Timeout;
                }
            }
            CloseHandle(event);
            result
        }
    }
}

impl Drop for HidDevice {
    fn drop(&mut self) {
        unsafe {
            CancelIoEx(self.handle, null());
            CloseHandle(self.handle);
        }
    }
}
