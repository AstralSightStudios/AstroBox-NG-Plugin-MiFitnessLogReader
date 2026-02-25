use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    astrobox::psys_host::{self, dialog, ui},
    extractor::{self, DeviceInfo, Platform},
};

pub const PICK_ANDROID_ZIP_EVENT: &str = "pick_android_zip";
pub const PICK_IOS_SQLITE_EVENT: &str = "pick_ios_sqlite";
pub const CLEAR_RESULT_EVENT: &str = "clear_result";

#[derive(Clone)]
struct UiState {
    root_element_id: Option<String>,
    status: String,
    devices: Vec<DeviceInfo>,
    source_file: Option<String>,
}

static UI_STATE: OnceLock<Mutex<UiState>> = OnceLock::new();

fn ui_state() -> &'static Mutex<UiState> {
    UI_STATE.get_or_init(|| {
        Mutex::new(UiState {
            root_element_id: None,
            status: "请选择 Android zip 或 iOS sqlite 文件".to_string(),
            devices: Vec::new(),
            source_file: None,
        })
    })
}

pub fn ui_event_processor(evtype: ui::Event, event: &str) {
    if evtype != ui::Event::Click {
        return;
    }

    match event {
        PICK_ANDROID_ZIP_EVENT => process_pick_and_extract(Platform::Android),
        PICK_IOS_SQLITE_EVENT => process_pick_and_extract(Platform::Ios),
        CLEAR_RESULT_EVENT => {
            {
                let mut state = ui_state()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.status = "已清空结果，请重新选择文件".to_string();
                state.devices.clear();
                state.source_file = None;
            }
            render_state();
        }
        _ => {}
    }
}

fn process_pick_and_extract(platform: Platform) {
    {
        let mut state = ui_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.status = format!("正在选择并处理 {} 文件...", platform.as_label());
    }
    render_state();

    let outcome = run_pick_and_extract(platform);

    {
        let mut state = ui_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        match outcome {
            Ok(outcome) => {
                let device_count = outcome.devices.len();
                state.status = match &outcome.detail {
                    Some(detail) => format!(
                        "解析完成：{} 设备 {} 台（{}）",
                        platform.as_label(),
                        device_count,
                        detail
                    ),
                    None => format!("解析完成：{} 设备 {} 台", platform.as_label(), device_count),
                };
                state.source_file = Some(outcome.source_file.display().to_string());
                state.devices = outcome.devices;
            }
            Err(err) => {
                state.status = format!("处理失败：{}", err);
            }
        }
    }

    render_state();
}

struct ExtractionOutcome {
    source_file: PathBuf,
    detail: Option<String>,
    devices: Vec<DeviceInfo>,
}

fn run_pick_and_extract(platform: Platform) -> Result<ExtractionOutcome, String> {
    let cwd = env::current_dir().map_err(|e| format!("获取当前目录失败: {}", e))?;
    let session_rel = PathBuf::from(".mifit_reader").join(format!(
        "{}_{}",
        match platform {
            Platform::Android => "android",
            Platform::Ios => "ios",
        },
        current_timestamp_ms()
    ));
    let session_dir = cwd.join(&session_rel);
    let pick_dir_rel = session_rel.join("picked");
    let pick_dir = cwd.join(&pick_dir_rel);
    fs::create_dir_all(&pick_dir)
        .map_err(|e| format!("创建临时目录失败 ({}): {}", pick_dir.display(), e))?;

    let pick_config = dialog::PickConfig {
        read: false,
        copy_to: Some(pick_dir_rel.to_string_lossy().to_string()),
    };

    let filter = dialog::FilterConfig {
        multiple: false,
        extensions: match platform {
            Platform::Android => vec!["zip".to_string()],
            Platform::Ios => vec!["sqlite".to_string(), "db".to_string()],
        },
        default_directory: "".to_string(),
        default_file_name: "".to_string(),
    };

    let pick_result = resolve_future(dialog::pick_file(&pick_config, &filter));
    let picked_name = pick_result.name.trim().to_string();
    if picked_name.is_empty() {
        return Err("未选择文件".to_string());
    }

    let picked_file_path = resolve_picked_file_path(&pick_dir, &picked_name, platform)?;

    match platform {
        Platform::Android => {
            let extract_root = session_dir.join("work");
            fs::create_dir_all(&extract_root).map_err(|e| {
                format!(
                    "创建安卓解压工作目录失败 ({}): {}",
                    extract_root.display(),
                    e
                )
            })?;
            let (devices, log_path) =
                extractor::parse_android_zip(&picked_file_path, &extract_root)?;
            Ok(ExtractionOutcome {
                source_file: picked_file_path,
                detail: Some(format!("日志 {}", log_path.display())),
                devices,
            })
        }
        Platform::Ios => {
            let devices = extractor::parse_ios_sqlite(&picked_file_path)?;
            Ok(ExtractionOutcome {
                source_file: picked_file_path,
                detail: None,
                devices,
            })
        }
    }
}

fn resolve_future<T>(future: wit_bindgen::FutureReader<T>) -> T {
    wit_bindgen::block_on(future.into_future())
}

fn resolve_picked_file_path(
    pick_dir: &Path,
    picked_name: &str,
    platform: Platform,
) -> Result<PathBuf, String> {
    let direct = PathBuf::from(picked_name);
    let mut candidates = vec![
        direct.clone(),
        pick_dir.join(picked_name),
        pick_dir.join(
            direct
                .file_name()
                .map(|x| x.to_os_string())
                .unwrap_or_default(),
        ),
    ];

    candidates.retain(|path| !path.as_os_str().is_empty());

    if let Some(found) = candidates.into_iter().find(|path| path.exists()) {
        return Ok(found);
    }

    let expected_exts = match platform {
        Platform::Android => vec!["zip"],
        Platform::Ios => vec!["sqlite", "db"],
    };
    let mut all_files = Vec::new();
    collect_files_recursive(pick_dir, &mut all_files);

    let mut matched: Vec<PathBuf> = all_files
        .into_iter()
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| {
                    expected_exts
                        .iter()
                        .any(|expected| ext.eq_ignore_ascii_case(expected))
                })
                .unwrap_or(false)
        })
        .collect();

    matched.sort_by(|a, b| {
        let am = a
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let bm = b
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        bm.cmp(&am)
    });

    matched.into_iter().next().ok_or_else(|| {
        format!(
            "未在 copy-to 目录中找到可读取文件（returned name: {}）",
            picked_name
        )
    })
}

fn collect_files_recursive(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

fn current_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn render_state() {
    let (root_id, state) = {
        let state = ui_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.root_element_id.clone(), state.clone())
    };
    if let Some(root_id) = root_id {
        psys_host::ui::render(&root_id, build_main_ui(&state));
    }
}

fn build_main_ui(state: &UiState) -> ui::Element {
    let title = ui::Element::new(ui::ElementType::P, Some("Mi Fitness 设备信息提取器")).size(26);
    let intro = ui::Element::new(
        ui::ElementType::P,
        Some("安卓请选择导出的 zip；iOS 请选择 manifest sqlite。"),
    );

    let android_btn = ui::Element::new(ui::ElementType::Button, Some("选择安卓 ZIP"))
        .bg("#2D7CF7")
        .text_color("#FFFFFF")
        .margin_right(8)
        .on(ui::Event::Click, PICK_ANDROID_ZIP_EVENT);

    let ios_btn = ui::Element::new(ui::ElementType::Button, Some("选择 iOS SQLite"))
        .bg("#138A5B")
        .text_color("#FFFFFF")
        .on(ui::Event::Click, PICK_IOS_SQLITE_EVENT);

    let clear_btn = ui::Element::new(ui::ElementType::Button, Some("清空结果"))
        .margin_top(8)
        .on(ui::Event::Click, CLEAR_RESULT_EVENT);

    let action_row = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .margin_top(12)
        .child(android_btn)
        .child(ios_btn);

    let status_text = ui::Element::new(ui::ElementType::P, Some(state.status.as_str()))
        .margin_top(12)
        .size(16);

    let source_text = match &state.source_file {
        Some(path) => ui::Element::new(
            ui::ElementType::P,
            Some(format!("来源文件: {}", path).as_str()),
        )
        .margin_top(8),
        None => ui::Element::new(ui::ElementType::P, Some("来源文件: -")).margin_top(8),
    };

    let list_title = ui::Element::new(ui::ElementType::P, Some("设备列表"))
        .size(20)
        .margin_top(16);
    let list = build_device_list(&state.devices);

    ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .padding(16)
        .child(title)
        .child(intro)
        .child(action_row)
        .child(clear_btn)
        .child(status_text)
        .child(source_text)
        .child(list_title)
        .child(list)
}

fn build_device_list(devices: &[DeviceInfo]) -> ui::Element {
    if devices.is_empty() {
        return ui::Element::new(ui::ElementType::P, Some("暂无结果"));
    }

    let mut container = ui::Element::new(ui::ElementType::Div, None).width_full();
    for item in devices {
        let name_line = format!("设备: {}", item.name);
        let key_line = format!("encryptKey: {}", item.encrypt_key);
        let source_line = format!("平台: {}", item.platform.as_label());

        let card = ui::Element::new(ui::ElementType::Div, None)
            .width_full()
            .padding(12)
            .margin_top(8)
            .radius(8)
            .border(1, "#D0D7DE")
            .child(ui::Element::new(ui::ElementType::P, Some(name_line.as_str())).size(18))
            .child(ui::Element::new(ui::ElementType::P, Some(key_line.as_str())).margin_top(6))
            .child(ui::Element::new(ui::ElementType::P, Some(source_line.as_str())).margin_top(6));

        container = container.child(card);
    }

    container
}

pub fn render_main_ui(element_id: &str) {
    {
        let mut state = ui_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.root_element_id = Some(element_id.to_string());
    }
    render_state();
}
