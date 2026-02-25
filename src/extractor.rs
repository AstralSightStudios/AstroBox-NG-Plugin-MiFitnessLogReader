use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
};

use nskeyedarchiver_converter::Converter;
use serde_json::Value as JsonValue;
use zip::ZipArchive;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Platform {
    Android,
    Ios,
}

impl Platform {
    pub fn as_label(self) -> &'static str {
        match self {
            Platform::Android => "Android",
            Platform::Ios => "iOS",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DeviceInfo {
    pub name: String,
    pub encrypt_key: String,
    pub platform: Platform,
}

pub fn parse_android_zip(
    zip_path: &Path,
    extract_root: &Path,
) -> Result<(Vec<DeviceInfo>, PathBuf), String> {
    if !zip_path.exists() {
        return Err(format!("找不到 zip 文件: {}", zip_path.display()));
    }

    let extract_dir = extract_root.join("android_unzip");
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)
            .map_err(|e| format!("清理旧解压目录失败 ({}): {}", extract_dir.display(), e))?;
    }
    fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("创建解压目录失败 ({}): {}", extract_dir.display(), e))?;

    unzip_to_dir(zip_path, &extract_dir)?;

    let log_path = find_file_by_name(&extract_dir, "XiaomiFit.main.log")
        .ok_or_else(|| "未在 zip 内找到 XiaomiFit.main.log".to_string())?;

    let log_text = fs::read_to_string(&log_path)
        .map_err(|e| format!("读取日志失败 ({}): {}", log_path.display(), e))?;

    let parsed = parse_android_log_text(&log_text);
    if parsed.is_empty() {
        return Err("未在日志中找到有效的设备 encryptKey 信息".to_string());
    }

    Ok((parsed, log_path))
}

pub fn parse_ios_sqlite(sqlite_path: &Path) -> Result<Vec<DeviceInfo>, String> {
    if !sqlite_path.exists() {
        return Err(format!("找不到 sqlite 文件: {}", sqlite_path.display()));
    }

    let sqlite_bytes = fs::read(sqlite_path)
        .map_err(|e| format!("读取 sqlite 失败 ({}): {}", sqlite_path.display(), e))?;

    let sqlite = SqliteFile::parse(sqlite_bytes)?;
    let manifest_root_page = sqlite.find_table_root_page("manifest")?;
    let manifest_rows = sqlite.read_table_rows(manifest_root_page)?;
    let inline_data = manifest_rows
        .iter()
        .find_map(|row| {
            let key = row.first().and_then(SqliteCellValue::as_text)?;
            if key == "registerList_cn" {
                row.get(3)
                    .and_then(SqliteCellValue::as_blob)
                    .map(|b| b.to_vec())
            } else {
                None
            }
        })
        .ok_or_else(|| "sqlite 中未找到 key=registerList_cn 的 inline_data".to_string())?;

    let mut converter = Converter::from_bytes(&inline_data)
        .map_err(|e| format!("解析 NSKeyedArchiver 二进制 plist 失败: {}", e))?;
    let decoded = converter
        .decode()
        .map_err(|e| format!("解码 NSKeyedArchiver 失败: {}", e))?;

    let mut raw_pairs = Vec::<(String, String)>::new();
    collect_pairs_from_plist(&decoded, &mut Vec::new(), &mut raw_pairs);
    let devices = dedup_pairs(raw_pairs, Platform::Ios);
    if devices.is_empty() {
        return Err("iOS 数据中未找到有效的设备 encryptKey 信息".to_string());
    }

    Ok(devices)
}

fn parse_android_log_text(log_text: &str) -> Vec<DeviceInfo> {
    let mut raw_pairs = Vec::<(String, String)>::new();
    for object_text in iter_json_objects(log_text) {
        let parsed = serde_json::from_str::<JsonValue>(&object_text);
        let Ok(json) = parsed else {
            continue;
        };
        collect_pairs_from_json(&json, &mut Vec::new(), &mut raw_pairs);
    }
    dedup_pairs(raw_pairs, Platform::Android)
}

fn dedup_pairs(raw_pairs: Vec<(String, String)>, platform: Platform) -> Vec<DeviceInfo> {
    let mut seen = HashSet::<(String, String)>::new();
    let mut out = Vec::<DeviceInfo>::new();

    for (name, encrypt_key) in raw_pairs {
        let normalized_name = normalize_name(&name).unwrap_or_else(|| "未知设备".to_string());
        let normalized_key = encrypt_key.to_ascii_lowercase();
        if !is_hex_32(&normalized_key) {
            continue;
        }

        if seen.insert((normalized_name.clone(), normalized_key.clone())) {
            out.push(DeviceInfo {
                name: normalized_name,
                encrypt_key: normalized_key,
                platform,
            });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.encrypt_key.cmp(&b.encrypt_key)));
    out
}

fn collect_pairs_from_json(
    value: &JsonValue,
    ancestor_names: &mut Vec<Option<String>>,
    out: &mut Vec<(String, String)>,
) {
    match value {
        JsonValue::Object(map) => {
            let this_name = extract_name_from_json_map(map);

            if let Some(encrypt_key) = extract_encrypt_key_from_json_map(map) {
                let name = this_name
                    .clone()
                    .or_else(|| nearest_name(ancestor_names))
                    .unwrap_or_else(|| "未知设备".to_string());
                out.push((name, encrypt_key));
            }

            ancestor_names.push(this_name);
            for child in map.values() {
                collect_pairs_from_json(child, ancestor_names, out);
            }
            ancestor_names.pop();
        }
        JsonValue::Array(items) => {
            for child in items {
                collect_pairs_from_json(child, ancestor_names, out);
            }
        }
        _ => {}
    }
}

fn collect_pairs_from_plist(
    value: &nskeyedarchiver_converter::plist::Value,
    ancestor_names: &mut Vec<Option<String>>,
    out: &mut Vec<(String, String)>,
) {
    use nskeyedarchiver_converter::plist::Value;

    match value {
        Value::Dictionary(map) => {
            let this_name = extract_name_from_plist_map(map);
            if let Some(encrypt_key) = extract_encrypt_key_from_plist_map(map) {
                let name = this_name
                    .clone()
                    .or_else(|| nearest_name(ancestor_names))
                    .unwrap_or_else(|| "未知设备".to_string());
                out.push((name, encrypt_key));
            }

            ancestor_names.push(this_name);
            for child in map.values() {
                collect_pairs_from_plist(child, ancestor_names, out);
            }
            ancestor_names.pop();
        }
        Value::Array(items) => {
            for child in items {
                collect_pairs_from_plist(child, ancestor_names, out);
            }
        }
        _ => {}
    }
}

fn nearest_name(ancestor_names: &[Option<String>]) -> Option<String> {
    ancestor_names.iter().rev().find_map(|n| n.clone())
}

fn extract_encrypt_key_from_json_map(map: &serde_json::Map<String, JsonValue>) -> Option<String> {
    for key in ["encryptKey", "encrypt_key", "encryptkey"] {
        if let Some(candidate) = map.get(key).and_then(|v| v.as_str()) {
            if is_hex_32(candidate) {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn extract_name_from_json_map(map: &serde_json::Map<String, JsonValue>) -> Option<String> {
    for key in [
        "name",
        "deviceName",
        "device_name",
        "productName",
        "product_name",
        "bltNamePrefix",
    ] {
        if let Some(raw) = map.get(key).and_then(|v| v.as_str()) {
            if let Some(normalized) = normalize_name(raw) {
                return Some(normalized);
            }
        }
    }
    None
}

fn extract_encrypt_key_from_plist_map(
    map: &nskeyedarchiver_converter::plist::Dictionary,
) -> Option<String> {
    use nskeyedarchiver_converter::plist::Value;

    for key in ["encryptKey", "encrypt_key", "encryptkey"] {
        if let Some(Value::String(candidate)) = map.get(key) {
            if is_hex_32(candidate) {
                return Some(candidate.clone());
            }
        }
    }
    None
}

fn extract_name_from_plist_map(
    map: &nskeyedarchiver_converter::plist::Dictionary,
) -> Option<String> {
    use nskeyedarchiver_converter::plist::Value;

    for key in [
        "name",
        "deviceName",
        "device_name",
        "productName",
        "product_name",
        "bltNamePrefix",
    ] {
        let Some(Value::String(raw)) = map.get(key) else {
            continue;
        };
        if let Some(normalized) = normalize_name(raw) {
            return Some(normalized);
        }
    }
    None
}

fn normalize_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "$null" {
        return None;
    }
    Some(trimmed.to_string())
}

fn is_hex_32(s: &str) -> bool {
    s.len() == 32 && s.as_bytes().iter().all(u8::is_ascii_hexdigit)
}

fn iter_json_objects(input: &str) -> Vec<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, &byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(idx);
                }
                depth = depth.saturating_add(1);
            }
            b'}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(begin) = start.take() {
                        out.push(input[begin..=idx].to_string());
                    }
                }
            }
            _ => {}
        }
    }

    out
}

fn unzip_to_dir(zip_path: &Path, dest_dir: &Path) -> Result<(), String> {
    let file = fs::File::open(zip_path)
        .map_err(|e| format!("打开 zip 失败 ({}): {}", zip_path.display(), e))?;
    let mut archive = ZipArchive::new(file).map_err(|e| format!("读取 zip 结构失败: {}", e))?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("读取 zip 条目失败 (index={}): {}", index, e))?;

        let enclosed = entry.enclosed_name().map(|p| p.to_path_buf());
        let Some(relative_path) = enclosed else {
            continue;
        };

        let out_path = dest_dir.join(relative_path);
        if entry.name().ends_with('/') {
            fs::create_dir_all(&out_path)
                .map_err(|e| format!("创建目录失败 ({}): {}", out_path.display(), e))?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败 ({}): {}", parent.display(), e))?;
        }

        let mut out_file = fs::File::create(&out_path)
            .map_err(|e| format!("创建解压文件失败 ({}): {}", out_path.display(), e))?;

        io::copy(&mut entry, &mut out_file)
            .map_err(|e| format!("解压文件失败 ({}): {}", out_path.display(), e))?;
    }

    Ok(())
}

fn find_file_by_name(root: &Path, file_name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_by_name(&path, file_name) {
                return Some(found);
            }
            continue;
        }

        if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name == file_name)
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
enum SqliteCellValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqliteCellValue {
    fn as_text(&self) -> Option<&str> {
        match self {
            SqliteCellValue::Text(v) => Some(v.as_str()),
            _ => None,
        }
    }

    fn as_blob(&self) -> Option<&[u8]> {
        match self {
            SqliteCellValue::Blob(v) => Some(v.as_slice()),
            _ => None,
        }
    }

    fn as_integer(&self) -> Option<i64> {
        match self {
            SqliteCellValue::Integer(v) => Some(*v),
            _ => None,
        }
    }
}

struct SqliteFile {
    bytes: Vec<u8>,
    page_size: usize,
    usable_size: usize,
}

#[derive(Clone, Copy)]
struct PageHeader {
    page_type: u8,
    cell_count: u16,
    cell_pointer_array_offset: usize,
    right_most_pointer: Option<u32>,
}

impl SqliteFile {
    fn parse(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() < 100 {
            return Err("sqlite 文件过小，不是有效 SQLite 文件".to_string());
        }
        if &bytes[0..16] != b"SQLite format 3\0" {
            return Err("文件头不是 SQLite format 3".to_string());
        }

        let page_size_raw = u16::from_be_bytes([bytes[16], bytes[17]]);
        let page_size = if page_size_raw == 1 {
            65536usize
        } else {
            page_size_raw as usize
        };
        let reserved_bytes = bytes[20] as usize;
        if page_size <= reserved_bytes + 4 {
            return Err("SQLite 页大小异常".to_string());
        }

        Ok(Self {
            bytes,
            page_size,
            usable_size: page_size - reserved_bytes,
        })
    }

    fn find_table_root_page(&self, table_name: &str) -> Result<u32, String> {
        let rows = self.read_table_rows(1)?;
        for row in rows {
            let Some(object_type) = row.first().and_then(SqliteCellValue::as_text) else {
                continue;
            };
            let Some(name) = row.get(1).and_then(SqliteCellValue::as_text) else {
                continue;
            };
            if object_type == "table" && name == table_name {
                let Some(root_page) = row.get(3).and_then(SqliteCellValue::as_integer) else {
                    continue;
                };
                if root_page <= 0 {
                    continue;
                }
                return Ok(root_page as u32);
            }
        }
        Err(format!("未在 sqlite_master 中找到表 {}", table_name))
    }

    fn read_table_rows(&self, root_page: u32) -> Result<Vec<Vec<SqliteCellValue>>, String> {
        let mut rows = Vec::new();
        self.walk_table_btree(root_page, &mut rows)?;
        Ok(rows)
    }

    fn walk_table_btree(
        &self,
        page_no: u32,
        out_rows: &mut Vec<Vec<SqliteCellValue>>,
    ) -> Result<(), String> {
        let page = self.page_data(page_no)?;
        let header = self.page_header(page_no)?;

        match header.page_type {
            0x0D => {
                for cell_index in 0..header.cell_count {
                    let ptr_offset = header.cell_pointer_array_offset + (cell_index as usize * 2);
                    let cell_offset = read_u16(page, ptr_offset)? as usize;
                    let row = self.parse_table_leaf_cell(page_no, cell_offset)?;
                    out_rows.push(row);
                }
            }
            0x05 => {
                for cell_index in 0..header.cell_count {
                    let ptr_offset = header.cell_pointer_array_offset + (cell_index as usize * 2);
                    let cell_offset = read_u16(page, ptr_offset)? as usize;
                    if cell_offset + 4 > page.len() {
                        return Err(format!(
                            "页 {} 内部节点 cell 偏移越界 (offset={})",
                            page_no, cell_offset
                        ));
                    }
                    let child_page = read_u32(page, cell_offset)?;
                    self.walk_table_btree(child_page, out_rows)?;
                }
                if let Some(right_page) = header.right_most_pointer {
                    self.walk_table_btree(right_page, out_rows)?;
                }
            }
            other => {
                return Err(format!(
                    "不支持的表 B-Tree 页类型 {:02X} (page={})",
                    other, page_no
                ));
            }
        }

        Ok(())
    }

    fn parse_table_leaf_cell(
        &self,
        page_no: u32,
        cell_offset: usize,
    ) -> Result<Vec<SqliteCellValue>, String> {
        let page = self.page_data(page_no)?;
        if cell_offset >= page.len() {
            return Err(format!(
                "页 {} 叶子节点 cell 偏移越界 (offset={})",
                page_no, cell_offset
            ));
        }

        let (payload_size, payload_varint_len) = read_varint(page, cell_offset)?;
        let (_, rowid_varint_len) = read_varint(page, cell_offset + payload_varint_len)?;

        let payload_size = payload_size as usize;
        let payload_start = cell_offset + payload_varint_len + rowid_varint_len;
        let local_payload_size = self.local_payload_size(payload_size);

        if payload_start + local_payload_size > page.len() {
            return Err(format!(
                "页 {} payload 越界 (start={}, local={}, page_len={})",
                page_no,
                payload_start,
                local_payload_size,
                page.len()
            ));
        }

        let mut payload = Vec::with_capacity(payload_size);
        payload.extend_from_slice(&page[payload_start..payload_start + local_payload_size]);

        if payload_size > local_payload_size {
            let overflow_ptr_pos = payload_start + local_payload_size;
            if overflow_ptr_pos + 4 > page.len() {
                return Err(format!(
                    "页 {} overflow 指针越界 (ptr_pos={})",
                    page_no, overflow_ptr_pos
                ));
            }
            let overflow_first_page = read_u32(page, overflow_ptr_pos)?;
            let remaining = payload_size - local_payload_size;
            let overflow_payload = self.read_overflow_chain(overflow_first_page, remaining)?;
            payload.extend_from_slice(&overflow_payload);
        }

        if payload.len() != payload_size {
            return Err(format!(
                "payload 长度不一致，expected={}, actual={}",
                payload_size,
                payload.len()
            ));
        }

        parse_sqlite_record(&payload)
    }

    fn read_overflow_chain(
        &self,
        mut page_no: u32,
        mut remaining: usize,
    ) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(remaining);
        while remaining > 0 {
            if page_no == 0 {
                return Err("overflow 链提前结束".to_string());
            }
            let page = self.page_data(page_no)?;
            if page.len() < 4 {
                return Err(format!("overflow 页 {} 长度异常", page_no));
            }

            let next_page = read_u32(page, 0)?;
            let max_chunk = self.usable_size.saturating_sub(4);
            if max_chunk == 0 {
                return Err("usable_size 异常，无法读取 overflow".to_string());
            }
            let chunk = remaining.min(max_chunk);
            if 4 + chunk > page.len() {
                return Err(format!(
                    "overflow 页 {} 数据越界 (chunk={}, page_len={})",
                    page_no,
                    chunk,
                    page.len()
                ));
            }
            out.extend_from_slice(&page[4..4 + chunk]);
            remaining -= chunk;
            page_no = next_page;
        }
        Ok(out)
    }

    fn local_payload_size(&self, payload_size: usize) -> usize {
        let max_local = self.usable_size - 35;
        let min_local = ((self.usable_size - 12) * 32 / 255) - 23;
        if payload_size <= max_local {
            return payload_size;
        }

        let mut local = min_local + ((payload_size - min_local) % (self.usable_size - 4));
        if local > max_local {
            local = min_local;
        }
        local
    }

    fn page_data(&self, page_no: u32) -> Result<&[u8], String> {
        if page_no == 0 {
            return Err("页号不能为 0".to_string());
        }
        let start = (page_no as usize - 1) * self.page_size;
        let end = start + self.page_size;
        if end > self.bytes.len() {
            return Err(format!(
                "页号越界 (page={}, page_size={}, file_len={})",
                page_no,
                self.page_size,
                self.bytes.len()
            ));
        }
        Ok(&self.bytes[start..end])
    }

    fn page_header(&self, page_no: u32) -> Result<PageHeader, String> {
        let page = self.page_data(page_no)?;
        let base = if page_no == 1 { 100 } else { 0 };
        if page.len() < base + 8 {
            return Err(format!("页头长度不足 (page={})", page_no));
        }

        let page_type = page[base];
        let cell_count = read_u16(page, base + 3)?;
        let header_len = match page_type {
            0x05 | 0x02 => 12usize,
            0x0D | 0x0A => 8usize,
            _ => {
                return Err(format!(
                    "未知 B-Tree 页类型 {:02X} (page={})",
                    page_type, page_no
                ));
            }
        };

        let right_most_pointer = if matches!(page_type, 0x05 | 0x02) {
            Some(read_u32(page, base + 8)?)
        } else {
            None
        };

        Ok(PageHeader {
            page_type,
            cell_count,
            cell_pointer_array_offset: base + header_len,
            right_most_pointer,
        })
    }
}

fn parse_sqlite_record(payload: &[u8]) -> Result<Vec<SqliteCellValue>, String> {
    let (header_size_raw, header_varint_len) = read_varint(payload, 0)?;
    let header_size = header_size_raw as usize;
    if header_size > payload.len() || header_size < header_varint_len {
        return Err("SQLite 记录头大小非法".to_string());
    }

    let mut serial_types = Vec::new();
    let mut cursor = header_varint_len;
    while cursor < header_size {
        let (serial, consumed) = read_varint(payload, cursor)?;
        serial_types.push(serial);
        cursor += consumed;
    }

    let mut data_cursor = header_size;
    let mut values = Vec::with_capacity(serial_types.len());
    for serial in serial_types {
        let (value, consumed) = parse_sqlite_serial_type(serial, &payload[data_cursor..])?;
        data_cursor += consumed;
        values.push(value);
    }

    Ok(values)
}

fn parse_sqlite_serial_type(
    serial_type: u64,
    data: &[u8],
) -> Result<(SqliteCellValue, usize), String> {
    match serial_type {
        0 => Ok((SqliteCellValue::Null, 0)),
        1 => read_signed(data, 1).map(|v| (SqliteCellValue::Integer(v), 1)),
        2 => read_signed(data, 2).map(|v| (SqliteCellValue::Integer(v), 2)),
        3 => read_signed(data, 3).map(|v| (SqliteCellValue::Integer(v), 3)),
        4 => read_signed(data, 4).map(|v| (SqliteCellValue::Integer(v), 4)),
        5 => read_signed(data, 6).map(|v| (SqliteCellValue::Integer(v), 6)),
        6 => read_signed(data, 8).map(|v| (SqliteCellValue::Integer(v), 8)),
        7 => {
            if data.len() < 8 {
                return Err("REAL 数据越界".to_string());
            }
            let raw = u64::from_be_bytes(data[0..8].try_into().unwrap_or_default());
            Ok((SqliteCellValue::Real(f64::from_bits(raw)), 8))
        }
        8 => Ok((SqliteCellValue::Integer(0), 0)),
        9 => Ok((SqliteCellValue::Integer(1), 0)),
        10 | 11 => Err("遇到保留的 SQLite serial type".to_string()),
        n if n >= 12 && n % 2 == 0 => {
            let len = ((n - 12) / 2) as usize;
            if data.len() < len {
                return Err("BLOB 数据越界".to_string());
            }
            Ok((SqliteCellValue::Blob(data[0..len].to_vec()), len))
        }
        n if n >= 13 && n % 2 == 1 => {
            let len = ((n - 13) / 2) as usize;
            if data.len() < len {
                return Err("TEXT 数据越界".to_string());
            }
            let text = String::from_utf8_lossy(&data[0..len]).to_string();
            Ok((SqliteCellValue::Text(text), len))
        }
        _ => Err("未知 SQLite serial type".to_string()),
    }
}

fn read_signed(data: &[u8], len: usize) -> Result<i64, String> {
    if data.len() < len {
        return Err("整数数据越界".to_string());
    }
    let mut value = 0i64;
    for byte in &data[0..len] {
        value = (value << 8) | i64::from(*byte);
    }
    let shift = (8usize.saturating_sub(len)) * 8;
    Ok((value << shift) >> shift)
}

fn read_varint(data: &[u8], offset: usize) -> Result<(u64, usize), String> {
    if offset >= data.len() {
        return Err("读取 varint 越界".to_string());
    }

    let mut value = 0u64;
    for index in 0..9usize {
        let pos = offset + index;
        if pos >= data.len() {
            return Err("读取 varint 越界".to_string());
        }
        let byte = data[pos];

        if index == 8 {
            value = (value << 8) | u64::from(byte);
            return Ok((value, 9));
        }

        value = (value << 7) | u64::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }

    Err("varint 读取失败".to_string())
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16, String> {
    if offset + 2 > data.len() {
        return Err("读取 u16 越界".to_string());
    }
    Ok(u16::from_be_bytes([data[offset], data[offset + 1]]))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32, String> {
    if offset + 4 > data.len() {
        return Err("读取 u32 越界".to_string());
    }
    Ok(u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_android_sample_log() {
        let content = fs::read_to_string("XiaomiFit-sample.main.log")
            .expect("failed to read XiaomiFit-sample.main.log");
        let devices = parse_android_log_text(&content);
        assert!(
            devices
                .iter()
                .any(|d| d.encrypt_key == "9e8da25999229b73203c539f736b3260"),
            "expected sample Android encryptKey to be present"
        );
    }

    #[test]
    fn parse_ios_sample_sqlite() {
        let devices = parse_ios_sqlite(Path::new("manifest-sample.sqlite"))
            .expect("failed to parse manifest-sample.sqlite");
        assert!(
            devices
                .iter()
                .any(|d| d.encrypt_key == "fd0ce943010e5112c6a35cb3ea61b968"),
            "expected sample iOS encryptKey to be present"
        );
    }
}
