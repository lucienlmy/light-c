// ============================================================================
// MFT 核心 — USN 枚举 + 路径重建（供 big_files 使用）
// ============================================================================

#![cfg(target_os = "windows")]

use std::collections::{HashMap, HashSet, VecDeque};

use winapi::shared::minwindef::{BOOL, DWORD, LPVOID};
use winapi::shared::ntdef::HANDLE;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING};
use winapi::um::handleapi::CloseHandle;
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
use winapi::um::securitybaseapi::GetTokenInformation;
use winapi::um::winnt::{
    TokenElevation, FILE_ATTRIBUTE_DIRECTORY, FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ,
    TOKEN_ELEVATION, TOKEN_QUERY,
};

extern "system" {
    fn GetVolumeInformationW(
        lpRootPathName: *const u16, lpVolumeNameBuffer: *mut u16, nVolumeNameSize: DWORD,
        lpVolumeSerialNumber: *mut DWORD, lpMaximumComponentLength: *mut DWORD,
        lpFileSystemFlags: *mut DWORD, lpFileSystemNameBuffer: *mut u16, nFileSystemNameSize: DWORD,
    ) -> BOOL;
}

pub const FSCTL_ENUM_USN_DATA: DWORD = (9 << 16) | (0 << 14) | (44 << 2) | 3;
pub const ERROR_HANDLE_EOF: DWORD = 38;
pub const USN_BUFFER_SIZE: usize = 1024 * 1024;

#[repr(C)]
#[allow(non_snake_case)]
pub struct MftEnumDataV0 { pub StartFileReferenceNumber: u64, pub LowUsn: i64, pub HighUsn: i64 }

#[derive(Clone)]
pub struct MftEntry { pub mft_id: u64, pub parent_id: u64, pub name: String, pub size: u64, pub is_dir: bool }

pub fn open_volume(drive_letter: char) -> Result<HANDLE, String> {
    let p = format!("\\\\.\\{}:", drive_letter);
    let w: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { CreateFileW(w.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    if h == winapi::um::handleapi::INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(format!("无法打开卷设备 {} (错误码: {})", p, err));
    }
    Ok(h)
}

pub fn close_volume(h: HANDLE) { unsafe { CloseHandle(h) }; }

pub fn enumerate_usn_records_v2(h_device: HANDLE, cb: &impl Fn(usize)) -> Result<Vec<MftEntry>, String> {
    let mut entries: Vec<MftEntry> = Vec::new();
    let mut buf: Vec<u8> = vec![0u8; USN_BUFFER_SIZE];
    let mut processed: usize = 0;
    let mut frn: u64 = 0;
    loop {
        let mut br: DWORD = 0;
        let mut ed = MftEnumDataV0 { StartFileReferenceNumber: frn, LowUsn: 0, HighUsn: i64::MAX };
        let ok = unsafe { DeviceIoControl(h_device, FSCTL_ENUM_USN_DATA, &mut ed as *mut _ as LPVOID, std::mem::size_of::<MftEnumDataV0>() as DWORD, buf.as_mut_ptr() as LPVOID, USN_BUFFER_SIZE as DWORD, &mut br, std::ptr::null_mut()) };
        if ok == 0 {
            if unsafe { GetLastError() } == ERROR_HANDLE_EOF { break; }
            return Err("DeviceIoControl 失败".into());
        }
        if br == 0 { break; }
        if processed == 0 { cb(0); }
        let mut offset: usize = 8;
        while offset < br as usize {
            let (rlen, mft_id, parent_id, file_name, is_dir) = unsafe {
                use std::ptr;
                let base = buf.as_ptr().add(offset);
                let rl = ptr::read_unaligned(base.add(0) as *const u32) as usize;
                let fr = ptr::read_unaligned(base.add(8) as *const u64);
                let pr = ptr::read_unaligned(base.add(16) as *const u64);
                let at = ptr::read_unaligned(base.add(52) as *const u32);
                let nl = ptr::read_unaligned(base.add(56) as *const u16) as usize / 2;
                let no = ptr::read_unaligned(base.add(58) as *const u16) as usize;
                let nm = if nl > 0 {
                    String::from_utf16_lossy(std::slice::from_raw_parts(buf.as_ptr().add(offset + no) as *const u16, nl))
                } else { String::new() };
                (rl, fr & 0x0000_FFFF_FFFF_FFFF, pr & 0x0000_FFFF_FFFF_FFFF, nm, (at & FILE_ATTRIBUTE_DIRECTORY) == FILE_ATTRIBUTE_DIRECTORY)
            };
            entries.push(MftEntry { mft_id, parent_id, name: file_name, size: 0, is_dir });
            processed += 1;
            if processed % 10_000 == 0 { cb(processed); }
            offset += rlen;
        }
        frn = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const u64) };
    }
    cb(processed);
    Ok(entries)
}

pub fn rebuild_full_paths(entries: &[MftEntry], drive_letter: char) -> HashMap<u64, String> {
    let cap = entries.len();
    let mut paths: HashMap<u64, String> = HashMap::with_capacity(cap);
    let mut children_map: HashMap<u64, Vec<usize>> = HashMap::with_capacity(cap);
    for (idx, e) in entries.iter().enumerate() { children_map.entry(e.parent_id).or_default().push(idx); }
    let mut queue: VecDeque<usize> = VecDeque::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let drive_root = format!("{}:\\", drive_letter);

    let root_mft_id: u64 = 5;
    paths.insert(root_mft_id, drive_root.clone());
    seen.insert(root_mft_id);
    if let Some(kids) = children_map.get(&root_mft_id) { for &c in kids { queue.push_back(c); } }
    for (_idx, e) in entries.iter().enumerate() {
        if e.parent_id == e.mft_id && seen.insert(e.mft_id) {
            paths.insert(e.mft_id, drive_root.clone());
            if let Some(kids) = children_map.get(&e.mft_id) { for &c in kids { queue.push_back(c); } }
        }
    }

    while let Some(idx) = queue.pop_front() {
        let e = &entries[idx];
        if !seen.insert(e.mft_id) { continue; }
        let parent_str = match paths.get(&e.parent_id) { Some(p) => p, None => continue };
        // String push_str 替代 format!，避免额外分配
        let mut full = String::with_capacity(parent_str.len() + 1 + e.name.len());
        full.push_str(parent_str);
        if !parent_str.ends_with('\\') { full.push('\\'); }
        full.push_str(&e.name);
        paths.insert(e.mft_id, full);
        if let Some(kids) = children_map.get(&e.mft_id) { for &c in kids { queue.push_back(c); } }
    }
    paths
}

pub fn is_elevated() -> bool {
    let mut t: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t) } == 0 { return false; }
    let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut s: DWORD = 0;
    let ok = unsafe { GetTokenInformation(t, TokenElevation, &mut e as *mut _ as LPVOID, std::mem::size_of::<TOKEN_ELEVATION>() as DWORD, &mut s) };
    unsafe { CloseHandle(t) };
    ok != 0 && e.TokenIsElevated != 0
}

pub fn is_ntfs(drive_letter: char) -> bool {
    let r = format!("{}:\\", drive_letter);
    let w: Vec<u16> = r.encode_utf16().chain(std::iter::once(0)).collect();
    let mut fs: [u16; 16] = [0; 16];
    if unsafe { GetVolumeInformationW(w.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), fs.as_mut_ptr(), fs.len() as DWORD) } == 0 { return false; }
    String::from_utf16_lossy(&fs).trim_end_matches('\0').eq_ignore_ascii_case("NTFS")
}
