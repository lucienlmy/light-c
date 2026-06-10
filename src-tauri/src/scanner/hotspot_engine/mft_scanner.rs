// ============================================================================
// MFT 直读扫描引擎（仅 Windows，需管理员权限 + NTFS 文件系统）
// 自包含实现，不依赖 mft_core
// ============================================================================

#![cfg(windows)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use log::info;

use crate::scanner::hotspot::FolderStats;

use winapi::shared::minwindef::{BOOL, DWORD, LPVOID};
use winapi::shared::ntdef::HANDLE;
use winapi::um::fileapi::CreateFileW;
use winapi::um::handleapi::CloseHandle;
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::fileapi::OPEN_EXISTING;
use winapi::um::winnt::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ,
};
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
use winapi::um::securitybaseapi::GetTokenInformation;
use winapi::um::winnt::{TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};

extern "system" {
    fn GetVolumeInformationW(
        lpRootPathName: *const u16, lpVolumeNameBuffer: *mut u16, nVolumeNameSize: DWORD,
        lpVolumeSerialNumber: *mut DWORD, lpMaximumComponentLength: *mut DWORD,
        lpFileSystemFlags: *mut DWORD, lpFileSystemNameBuffer: *mut u16, nFileSystemNameSize: DWORD,
    ) -> BOOL;
}

const FSCTL_ENUM_USN_DATA: DWORD = (9 << 16) | (44 << 2) | (0 << 14);
const ERROR_HANDLE_EOF: DWORD = 38;
const USN_BUFFER_SIZE: usize = 65536;

#[repr(C)]
#[allow(non_snake_case)]
struct MftEnumData { StartFileReferenceNumber: u64, LowUsn: i64, HighUsn: i64 }

#[repr(C)]
#[allow(non_snake_case)]
struct UsnRecordV2 {
    RecordLength: u32, MajorVersion: u16, MinorVersion: u16,
    FileReferenceNumber: u64, ParentFileReferenceNumber: u64,
    Usn: i64, TimeStamp: i64, Reason: u32, SourceInfo: u32, SecurityId: u32,
    FileAttributes: u32, FileNameLength: u16, FileNameOffset: u16, FileName: [u16; 1],
}

struct MftEntry { mft_id: u64, parent_id: u64, name: String, is_dir: bool }

pub fn scan_via_mft(drive_letter: char, progress_cb: impl Fn(usize)) -> Result<HashMap<PathBuf, FolderStats>, String> {
    info!("[MFT] 开始扫描驱动器 {}: — 直接读取 NTFS Master File Table", drive_letter);
    let volume_path = format!("\\\\.\\{}:", drive_letter);
    let volume_path_wide: Vec<u16> = volume_path.encode_utf16().chain(std::iter::once(0)).collect();
    let h_device = unsafe { CreateFileW(volume_path_wide.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    if h_device == winapi::um::handleapi::INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(format!("[MFT] 无法打开卷设备 {} (错误码: {})", volume_path, err));
    }
    info!("[MFT] 已打开卷设备 {}", volume_path);
    let entries = enumerate_usn_records(h_device, &progress_cb)?;
    unsafe { CloseHandle(h_device) };
    info!("[MFT] USN 枚举完成，共 {} 条记录，开始重建路径...", entries.len());
    let paths = rebuild_full_paths(&entries, drive_letter);
    info!("[MFT] 路径重建完成，开始向上聚合大小...");
    let mut folder_map: HashMap<PathBuf, FolderStats> = HashMap::new();
    for entry in &entries {
        if entry.is_dir { continue; }
        let file_path = match paths.get(&entry.mft_id) { Some(p) => p, None => continue };
        let mut current = file_path.parent().map(|p| p.to_path_buf());
        while let Some(dir_path) = current {
            let stats = folder_map.entry(dir_path.clone()).or_insert(FolderStats { total_size: 0, file_count: 0, last_modified: 0 });
            stats.file_count += 1;
            current = dir_path.parent().map(|p| p.to_path_buf());
        }
    }
    info!("[MFT] 扫描完成: {} 个文件, {} 个目录", entries.iter().filter(|e| !e.is_dir).count(), folder_map.len());
    Ok(folder_map)
}

fn enumerate_usn_records(h_device: HANDLE, progress_cb: &impl Fn(usize)) -> Result<Vec<MftEntry>, String> {
    let mut entries: Vec<MftEntry> = Vec::new();
    let mut buf: Vec<u8> = vec![0u8; USN_BUFFER_SIZE];
    let mut processed: usize = 0;
    let mut mft_enum_data = MftEnumData { StartFileReferenceNumber: 0, LowUsn: 0, HighUsn: i64::MAX };
    loop {
        let mut bytes_returned: DWORD = 0;
        let success = unsafe { DeviceIoControl(h_device, FSCTL_ENUM_USN_DATA, &mut mft_enum_data as *mut MftEnumData as LPVOID, std::mem::size_of::<MftEnumData>() as DWORD, buf.as_mut_ptr() as LPVOID, USN_BUFFER_SIZE as DWORD, &mut bytes_returned, std::ptr::null_mut()) };
        if success == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_HANDLE_EOF { break; }
            return Err(format!("[MFT] DeviceIoControl 失败，错误码: {}", err));
        }
        if bytes_returned == 0 { break; }
        let mut offset: usize = 0;
        while offset < bytes_returned as usize {
            let rec_ptr = unsafe { buf.as_ptr().add(offset) as *const UsnRecordV2 };
            let rec = unsafe { &*rec_ptr };
            let record_length = rec.RecordLength as usize;
            if record_length == 0 { break; }
            let mft_id = rec.FileReferenceNumber & 0x0000_FFFF_FFFF_FFFF;
            let parent_id = rec.ParentFileReferenceNumber & 0x0000_FFFF_FFFF_FFFF;
            let file_name_len = rec.FileNameLength as usize / 2;
            let file_name = if file_name_len > 0 {
                let name_slice = unsafe { std::slice::from_raw_parts(buf.as_ptr().add(offset + rec.FileNameOffset as usize) as *const u16, file_name_len) };
                String::from_utf16_lossy(name_slice)
            } else { String::new() };
            let is_dir = (rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) == FILE_ATTRIBUTE_DIRECTORY;
            entries.push(MftEntry { mft_id, parent_id, name: file_name, is_dir });
            processed += 1;
            if processed % 100_000 == 0 { progress_cb(processed); }
            mft_enum_data.StartFileReferenceNumber = rec.FileReferenceNumber;
            offset += record_length;
        }
    }
    progress_cb(processed);
    Ok(entries)
}

fn rebuild_full_paths(entries: &[MftEntry], drive_letter: char) -> HashMap<u64, PathBuf> {
    let mut paths: HashMap<u64, PathBuf> = HashMap::new();
    let mut children_map: HashMap<u64, Vec<usize>> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() { children_map.entry(entry.parent_id).or_default().push(idx); }
    let mut queue: VecDeque<usize> = VecDeque::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let drive_root = format!("{}:\\", drive_letter);
    for (_idx, entry) in entries.iter().enumerate() {
        if entry.parent_id == entry.mft_id {
            if seen.insert(entry.mft_id) {
                paths.insert(entry.mft_id, PathBuf::from(&drive_root));
                if let Some(children) = children_map.get(&entry.mft_id) { for &child_idx in children { queue.push_back(child_idx); } }
            }
        }
    }
    while let Some(idx) = queue.pop_front() {
        let entry = &entries[idx];
        if !seen.insert(entry.mft_id) { continue; }
        let parent_path = match paths.get(&entry.parent_id) { Some(p) => p.clone(), None => continue };
        let full_path = parent_path.join(&entry.name);
        paths.insert(entry.mft_id, full_path);
        if let Some(children) = children_map.get(&entry.mft_id) { for &child_idx in children { queue.push_back(child_idx); } }
    }
    paths
}

pub fn is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    let success = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if success == 0 { return false; }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret_size: DWORD = 0;
    let success = unsafe { GetTokenInformation(token, TokenElevation, &mut elevation as *mut TOKEN_ELEVATION as LPVOID, std::mem::size_of::<TOKEN_ELEVATION>() as DWORD, &mut ret_size) };
    unsafe { CloseHandle(token) };
    if success == 0 { return false; }
    elevation.TokenIsElevated != 0
}

pub fn is_ntfs(drive_letter: char) -> bool {
    let root_path = format!("{}:\\", drive_letter);
    let root_wide: Vec<u16> = root_path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut fs_name_buf: [u16; 16] = [0u16; 16];
    let success = unsafe { GetVolumeInformationW(root_wide.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), fs_name_buf.as_mut_ptr(), fs_name_buf.len() as DWORD) };
    if success == 0 { return false; }
    let fs_name = String::from_utf16_lossy(&fs_name_buf);
    fs_name.trim_end_matches('\0').eq_ignore_ascii_case("NTFS")
}
