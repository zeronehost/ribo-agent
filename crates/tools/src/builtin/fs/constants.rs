/// read 输出行数上限。
pub(crate) const MAX_LINES: usize = 1000;
/// read 输出字节上限（与 MAX_LINES 先到者生效）。
pub(crate) const MAX_BYTES: usize = 256 * 1024;
/// ls 深度参数硬上限。
pub(crate) const LS_MAX_DEPTH: usize = 5;
/// ls 条目数默认上限。
pub(crate) const LS_MAX_ENTRIES: usize = 500;
/// grep 跳过的大文件阈值（字节）。
pub(crate) const GREP_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// grep 目录递归深度上限。
pub(crate) const GREP_MAX_DEPTH: usize = 10;
