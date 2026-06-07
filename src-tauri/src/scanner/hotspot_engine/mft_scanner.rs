// ============================================================================
// MFT 直读扫描引擎（仅 Windows，需管理员权限 + NTFS 文件系统）
//
// 通过直接读取 NTFS Master File Table 获取全盘文件列表，
// 完全绕过文件系统 API，实现类似 WizTree 的秒级全盘扫描。
//
// 核心流程：
// 1. 打开 \\.\X: 卷设备
// 2. 通过 FSCTL_ENUM_USN_DATA 枚举所有文件记录
// 3. BFS 重建全路径
// 4. 向祖先目录累加 total_size 和 file_count
// ============================================================================

#![cfg(windows)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use log::info;

use crate::scanner::hotspot::FolderStats;

// ============================================================================
// Windows FFI 声明（手动定义，避免引入额外的 crate features）
// ============================================================================

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

// winapi 没有直接提供 USN 相关定义，手动声明
extern "system" {
    fn GetVolumeInformationW(
        lpRootPathName: *const u16,
        lpVolumeNameBuffer: *mut u16,
        nVolumeNameSize: DWORD,
        lpVolumeSerialNumber: *mut DWORD,
        lpMaximumComponentLength: *mut DWORD,
        lpFileSystemFlags: *mut DWORD,
        lpFileSystemNameBuffer: *mut u16,
        nFileSystemNameSize: DWORD,
    ) -> BOOL;
}

// ============================================================================
// NTFS USN Journal 常量
// ============================================================================

/// FSCTL_ENUM_USN_DATA 控制码
/// CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 44, METHOD_NEITHER, FILE_READ_DATA)
const FSCTL_ENUM_USN_DATA: DWORD = (9 << 16) | (44 << 2) | (0 << 14);

/// ERROR_HANDLE_EOF — USN 枚举正常结束
const ERROR_HANDLE_EOF: DWORD = 38;

/// 单次 DeviceIoControl 的缓冲区大小
const USN_BUFFER_SIZE: usize = 65536; // 64KB

// ============================================================================
// NTFS 数据结构（手动 #[repr(C)] 定义）
// ============================================================================

/// MFT_ENUM_DATA_V0 — 传递给 FSCTL_ENUM_USN_DATA 的输入参数
#[repr(C)]
#[allow(non_snake_case)]
struct MftEnumData {
    StartFileReferenceNumber: u64,
    LowUsn: i64,
    HighUsn: i64,
}

/// USN_RECORD_V2 — FSCTL_ENUM_USN_DATA 返回的每条记录
#[repr(C)]
#[allow(non_snake_case)]
struct UsnRecordV2 {
    RecordLength: u32,
    MajorVersion: u16,
    MinorVersion: u16,
    FileReferenceNumber: u64,
    ParentFileReferenceNumber: u64,
    Usn: i64,
    TimeStamp: i64,
    Reason: u32,
    SourceInfo: u32,
    SecurityId: u32,
    FileAttributes: u32,
    FileNameLength: u16,
    FileNameOffset: u16,
    FileName: [u16; 1], // 变长数组，实际通过 FileNameLength 读取
}

// ============================================================================
// MFT 条目（解析后的内存结构）
// ============================================================================

/// 从 USN_RECORD_V2 解析出的精简条目
struct MftEntry {
    /// 去序列号后的 MFT 文件 ID
    mft_id: u64,
    /// 去序列号后的父目录 MFT ID
    parent_id: u64,
    /// 文件名（从 UTF-16LE 解码）
    name: String,
    /// 文件大小（字节）
    size: u64,
    /// 是否为目录
    is_dir: bool,
}

// ============================================================================
// 扫描入口
// ============================================================================

/// 通过 MFT 直读扫描指定驱动器
///
/// # 参数
/// - `drive_letter`: 驱动器盘符，如 `'C'`
/// - `progress_cb`: 进度回调，参数为已处理的记录条数
///
/// # 返回
/// `HashMap<PathBuf, FolderStats>` — 目录全路径 → 聚合统计
pub fn scan_via_mft(
    drive_letter: char,
    progress_cb: impl Fn(usize),
) -> Result<HashMap<PathBuf, FolderStats>, String> {
    info!(
        "[MFT] 开始扫描驱动器 {}: — 直接读取 NTFS Master File Table",
        drive_letter
    );

    // Step 1: 打开卷设备
    let volume_path = format!("\\\\.\\{}:", drive_letter);
    let volume_path_wide: Vec<u16> = volume_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let h_device = unsafe {
        CreateFileW(
            volume_path_wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };

    if h_device == winapi::um::handleapi::INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(format!(
            "[MFT] 无法打开卷设备 {} (错误码: {})",
            volume_path, err
        ));
    }
    info!("[MFT] 已打开卷设备 {}", volume_path);

    // Step 2: FSCTL_ENUM_USN_DATA 枚举所有记录
    let entries = enumerate_usn_records(h_device, &progress_cb)?;

    unsafe { CloseHandle(h_device) };
    info!(
        "[MFT] USN 枚举完成，共 {} 条记录，开始重建路径...",
        entries.len()
    );

    // Step 3: BFS 重建全路径
    let paths = rebuild_full_paths(&entries, drive_letter);

    info!("[MFT] 路径重建完成，开始向上聚合大小...");

    // Step 4: 遍历文件条目，向祖先目录累加 total_size 和 file_count
    let mut folder_map: HashMap<PathBuf, FolderStats> = HashMap::new();

    for entry in &entries {
        if entry.is_dir {
            continue; // 目录不计入自身大小
        }

        // 查找该文件的完整路径
        let file_path = match paths.get(&entry.mft_id) {
            Some(p) => p,
            None => continue, // 路径重建失败，跳过
        };

        // 向上逐级找到所有祖先目录并累加
        let mut current = file_path.parent().map(|p| p.to_path_buf());
        while let Some(dir_path) = current {
            let stats = folder_map
                .entry(dir_path.clone())
                .or_insert_with(|| FolderStats {
                    total_size: 0,
                    file_count: 0,
                    last_modified: 0,
                });
            stats.total_size += entry.size;
            stats.file_count += 1;

            // 继续向上一级
            current = dir_path.parent().map(|p| p.to_path_buf());
        }
    }

    info!(
        "[MFT] 扫描完成: {} 个文件, {} 个目录",
        entries.iter().filter(|e| !e.is_dir).count(),
        folder_map.len()
    );

    Ok(folder_map)
}

// ============================================================================
// USN 记录枚举
// ============================================================================

fn enumerate_usn_records(
    h_device: HANDLE,
    progress_cb: &impl Fn(usize),
) -> Result<Vec<MftEntry>, String> {
    let mut entries: Vec<MftEntry> = Vec::new();
    let mut buf: Vec<u8> = vec![0u8; USN_BUFFER_SIZE];
    let mut processed: usize = 0;

    // 从 FRN 0 开始枚举
    let mut mft_enum_data = MftEnumData {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: i64::MAX,
    };

    loop {
        let mut bytes_returned: DWORD = 0;

        let success = unsafe {
            DeviceIoControl(
                h_device,
                FSCTL_ENUM_USN_DATA,
                &mut mft_enum_data as *mut MftEnumData as LPVOID,
                std::mem::size_of::<MftEnumData>() as DWORD,
                buf.as_mut_ptr() as LPVOID,
                USN_BUFFER_SIZE as DWORD,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };

        if success == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_HANDLE_EOF {
                // 枚举正常结束
                break;
            }
            return Err(format!("[MFT] DeviceIoControl 失败，错误码: {}", err));
        }

        if bytes_returned == 0 {
            break;
        }

        // 解析返回的 USN 记录
        let mut offset: usize = 0;
        while offset < bytes_returned as usize {
            // 安全：USN_RECORD_V2 由 Windows 内核填充，数据有效
            let rec_ptr = unsafe { buf.as_ptr().add(offset) as *const UsnRecordV2 };
            let rec = unsafe { &*rec_ptr };

            let record_length = rec.RecordLength as usize;
            if record_length == 0 {
                break;
            }

            let mft_id = rec.FileReferenceNumber & 0x0000_FFFF_FFFF_FFFF;
            let parent_id = rec.ParentFileReferenceNumber & 0x0000_FFFF_FFFF_FFFF;
            let file_name_len = rec.FileNameLength as usize / 2; // 字节 → 字符数

            // 安全：文件名由内核保证长度+偏移合法
            let file_name = if file_name_len > 0 {
                let name_slice = unsafe {
                    std::slice::from_raw_parts(
                        buf.as_ptr().add(offset + rec.FileNameOffset as usize) as *const u16,
                        file_name_len,
                    )
                };
                String::from_utf16_lossy(name_slice)
            } else {
                String::new()
            };

            let is_dir =
                (rec.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) == FILE_ATTRIBUTE_DIRECTORY;

            entries.push(MftEntry {
                mft_id,
                parent_id,
                name: file_name,
                size: 0, // USN_RECORD_V2 不含文件大小，后续需要时可扩展
                is_dir,
            });

            processed += 1;
            if processed % 100_000 == 0 {
                progress_cb(processed);
            }

            // 更新下次查询的起始 FRN
            mft_enum_data.StartFileReferenceNumber = rec.FileReferenceNumber;
            offset += record_length;
        }
    }

    // 最终进度通知
    progress_cb(processed);
    Ok(entries)
}

// ============================================================================
// BFS 全路径重建
// ============================================================================

fn rebuild_full_paths(
    entries: &[MftEntry],
    drive_letter: char,
) -> HashMap<u64, PathBuf> {
    let mut paths: HashMap<u64, PathBuf> = HashMap::new();
    let mut children_map: HashMap<u64, Vec<usize>> = HashMap::new(); // parent_id → child indices

    // 建立父子索引
    for (idx, entry) in entries.iter().enumerate() {
        children_map
            .entry(entry.parent_id)
            .or_default()
            .push(idx);
    }

    // BFS 从根目录出发
    let mut queue: VecDeque<usize> = VecDeque::new();
    let mut seen: HashSet<u64> = HashSet::new();

    let drive_root = format!("{}:\\", drive_letter);

    for (_idx, entry) in entries.iter().enumerate() {
        // 根目录：parent_id == mft_id（MFT 中的根条目指向自身）
        if entry.parent_id == entry.mft_id {
            if seen.insert(entry.mft_id) {
                paths.insert(entry.mft_id, PathBuf::from(&drive_root));
                if let Some(children) = children_map.get(&entry.mft_id) {
                    for &child_idx in children {
                        queue.push_back(child_idx);
                    }
                }
            }
        }
    }

    // BFS 遍历
    while let Some(idx) = queue.pop_front() {
        let entry = &entries[idx];

        // 检测环
        if !seen.insert(entry.mft_id) {
            continue;
        }

        // 获取父目录路径
        let parent_path = match paths.get(&entry.parent_id) {
            Some(p) => p.clone(),
            None => continue, // 父路径未知，跳过
        };

        let full_path = parent_path.join(&entry.name);
        paths.insert(entry.mft_id, full_path);

        // 将子节点加入队列
        if let Some(children) = children_map.get(&entry.mft_id) {
            for &child_idx in children {
                queue.push_back(child_idx);
            }
        }
    }

    paths
}

// ============================================================================
// 权限 & 文件系统检测
// ============================================================================

/// 检测当前进程是否以管理员身份运行
pub fn is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    let success = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        )
    };
    if success == 0 {
        return false;
    }

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret_size: DWORD = 0;

    let success = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as LPVOID,
            std::mem::size_of::<TOKEN_ELEVATION>() as DWORD,
            &mut ret_size,
        )
    };

    unsafe { CloseHandle(token) };

    if success == 0 {
        return false;
    }

    elevation.TokenIsElevated != 0
}

/// 检测指定驱动器是否为 NTFS 文件系统
pub fn is_ntfs(drive_letter: char) -> bool {
    let root_path = format!("{}:\\", drive_letter);
    let root_wide: Vec<u16> = root_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut fs_name_buf: [u16; 16] = [0u16; 16];

    let success = unsafe {
        GetVolumeInformationW(
            root_wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_name_buf.as_mut_ptr(),
            fs_name_buf.len() as DWORD,
        )
    };

    if success == 0 {
        return false;
    }

    let fs_name = String::from_utf16_lossy(&fs_name_buf);
    let fs_name = fs_name.trim_end_matches('\0');
    fs_name.eq_ignore_ascii_case("NTFS")
}
