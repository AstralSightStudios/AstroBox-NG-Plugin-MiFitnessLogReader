use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    astrobox::psys_host::{self, dialog, ui},
    extractor::{self, DeviceInfo, Platform},
    resources::{TUTORIAL_VID_ANDROID, TUTORIAL_VID_IOS},
};

pub const PICK_ANDROID_ZIP_EVENT: &str = "pick_android_zip";
pub const PICK_IOS_SQLITE_EVENT: &str = "pick_ios_sqlite";
pub const CLEAR_RESULT_EVENT: &str = "clear_result";
pub const TAB_EXTRACT_EVENT: &str = "tab_extract";
pub const TAB_TUTORIAL_EVENT: &str = "tab_tutorial";

const KEY_INPUT_LOCK_PREFIX: &str = "lock_key_";

// --- UI 颜色定义 ---
const COLOR_BG_CARD: &str = "#1C1C1E";       // 卡片背景，比底层背景亮一点，产生浮动感
const COLOR_BG_TAB_WRAP: &str = "#242426";
const COLOR_BG_TAB_ACTIVE: &str = "#3A3A3C";
const COLOR_BG_TAB_IDLE: &str = "transparent";
const COLOR_BG_BADGE: &str = "#2C2C2E";
const COLOR_BG_INPUT: &str = "#000000";

const COLOR_BG_BTN_PRIMARY: &str = "#0A84FF"; // 使用亮蓝色作为主行为，引导视觉
const COLOR_TEXT_BTN_PRIMARY: &str = "#FFFFFF";
const COLOR_BG_BTN_SECONDARY: &str = "#2C2C2E";
const COLOR_TEXT_BTN_SECONDARY: &str = "#FFFFFF";
const COLOR_BG_BTN_DANGER: &str = "#3A1A1E";  // 危险操作用微红底色，避免镂空太生硬
const COLOR_TEXT_BTN_DANGER: &str = "#FF453A";

const COLOR_BORDER_SOFT: &str = "#38383A";    // 稍微提亮边框，增加精致感
const COLOR_BORDER_DANGER: &str = "#FF453A";

const COLOR_TEXT_PRIMARY: &str = "#FFFFFF";
const COLOR_TEXT_SECONDARY: &str = "#98989D";
const COLOR_TEXT_MUTED: &str = "#636366";
const COLOR_TEXT_SUCCESS: &str = "#32D74B";
const COLOR_TEXT_WARN: &str = "#FFD60A";
const COLOR_TEXT_DANGER: &str = "#FF453A";

#[derive(Clone, Copy, Eq, PartialEq)]
enum UiTab {
    Extract,
    Tutorial,
}

#[derive(Clone)]
struct UiState {
    root_element_id: Option<String>,
    status: String,
    devices: Vec<DeviceInfo>,
    source_file: Option<String>,
    active_tab: UiTab,
}

static UI_STATE: OnceLock<Mutex<UiState>> = OnceLock::new();

fn ui_state() -> &'static Mutex<UiState> {
    UI_STATE.get_or_init(|| {
        Mutex::new(UiState {
            root_element_id: None,
            status: "请选择 Android zip 或 iOS sqlite 文件".to_string(),
            devices: Vec::new(),
            source_file: None,
            active_tab: UiTab::Extract,
        })
    })
}

pub fn ui_event_processor(evtype: ui::Event, event: &str) {
    match evtype {
        ui::Event::Click => match event {
            PICK_ANDROID_ZIP_EVENT => process_pick_and_extract(Platform::Android),
            PICK_IOS_SQLITE_EVENT => process_pick_and_extract(Platform::Ios),
            TAB_EXTRACT_EVENT => {
                {
                    let mut state = ui_state()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.active_tab = UiTab::Extract;
                }
                render_state();
            }
            TAB_TUTORIAL_EVENT => {
                {
                    let mut state = ui_state()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.active_tab = UiTab::Tutorial;
                }
                render_state();
            }
            CLEAR_RESULT_EVENT => {
                {
                    let mut state = ui_state()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.status = "已清空结果，请重新选择文件".to_string();
                    state.devices.clear();
                    state.source_file = None;
                    state.active_tab = UiTab::Extract;
                }
                render_state();
            }
            _ => {}
        },
        ui::Event::Input | ui::Event::Change => {
            if event.starts_with(KEY_INPUT_LOCK_PREFIX) {
                render_state();
            }
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
        state.active_tab = UiTab::Extract;
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
            Platform::Ios => vec![],
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

// ======================== UI 构建逻辑重构 ========================

fn build_main_ui(state: &UiState) -> ui::Element {
    let title = ui::Element::new(ui::ElementType::P, Some("Mi Fitness Extractor"))
        .size(24) // 增大主标题字号
        .text_color(COLOR_TEXT_PRIMARY);
    let subtitle = ui::Element::new(
        ui::ElementType::P,
        Some("提取 Mi Fitness 导出文件中的设备名与 encryptKey"),
    )
    .margin_top(6)
    .size(14)
    .text_color(COLOR_TEXT_SECONDARY);

    let header = ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .margin_bottom(10)
        .child(title)
        .child(subtitle);

    let tab_bar = build_tab_bar(state.active_tab);
    let tab_content = match state.active_tab {
        UiTab::Extract => build_extract_tab(state),
        UiTab::Tutorial => build_tutorial_tab(),
    };

    ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .padding(20) // 增加全局外边距，让整体呼吸感更强
        .child(header)
        .child(tab_bar)
        .child(tab_content)
}

fn build_tab_bar(active_tab: UiTab) -> ui::Element {
    let extract_tab = build_single_tab(
        "提取工具",
        TAB_EXTRACT_EVENT,
        active_tab == UiTab::Extract,
        true,
    );
    let tutorial_tab = build_single_tab(
        "使用教程",
        TAB_TUTORIAL_EVENT,
        active_tab == UiTab::Tutorial,
        false,
    );

    let tab_wrap = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .align_center()
        .bg(COLOR_BG_TAB_WRAP)
        .radius(10) // 更圆润的胶囊包裹
        .padding(4)
        .child(extract_tab)
        .child(tutorial_tab);

    // 将 Tab 整体居中，更符合工具类 App 的现代布局
    ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .flex()
        .justify_center()
        .margin_top(16)
        .margin_bottom(24)
        .child(tab_wrap)
}

fn build_single_tab(label: &str, event_id: &str, active: bool, first: bool) -> ui::Element {
    let (bg, text_color) = if active {
        (COLOR_BG_TAB_ACTIVE, COLOR_TEXT_PRIMARY)
    } else {
        (COLOR_BG_TAB_IDLE, COLOR_TEXT_MUTED)
    };

    let mut tab = ui::Element::new(ui::ElementType::Button, Some(label))
        .without_default_styles()
        .on(ui::Event::Click, event_id)
        .bg(bg)
        .radius(8)
        .padding_top(8)
        .padding_bottom(8)
        .padding_left(20)
        .padding_right(20)
        .size(13)
        .text_color(text_color)
        .transition("all 0.2s ease");

    if first {
        tab = tab.margin_right(4);
    }
    tab
}

fn build_action_button(
    label: &str,
    event_id: &str,
    bg: &str,
    border_col: &str,
    text_color: &str,
) -> ui::Element {
    let mut btn = ui::Element::new(ui::ElementType::Button, Some(label))
        .without_default_styles()
        .on(ui::Event::Click, event_id)
        .bg(bg)
        .radius(10) // 提升圆角
        .padding_top(10)
        .padding_bottom(10)
        .padding_left(16)
        .padding_right(16)
        .size(13)
        .text_color(text_color)
        .transition("all 0.2s ease");

    if border_col != "transparent" {
        btn = btn.border(1, border_col);
    }
    btn
}

fn status_text_color(status: &str) -> &'static str {
    if status.starts_with("处理失败") {
        COLOR_TEXT_DANGER
    } else if status.starts_with("正在") {
        COLOR_TEXT_WARN
    } else if status.starts_with("解析完成") {
        COLOR_TEXT_SUCCESS
    } else {
        COLOR_TEXT_PRIMARY
    }
}

fn build_extract_tab(state: &UiState) -> ui::Element {
    let android_btn = build_action_button(
        "选择 Android ZIP",
        PICK_ANDROID_ZIP_EVENT,
        COLOR_BG_BTN_PRIMARY,   // 亮蓝色，强调主操作
        "transparent",
        COLOR_TEXT_BTN_PRIMARY,
    )
    .margin_right(10);

    let ios_btn = build_action_button(
        "选择 iOS SQLite",
        PICK_IOS_SQLITE_EVENT,
        COLOR_BG_BTN_SECONDARY,
        "transparent",
        COLOR_TEXT_BTN_SECONDARY,
    )
    .margin_right(10);

    let clear_btn = build_action_button(
        "清空",
        CLEAR_RESULT_EVENT,
        COLOR_BG_BTN_DANGER,    // 微红底色
        "transparent",          // 拿掉红色边框，显得没那么扎眼
        COLOR_TEXT_BTN_DANGER,
    );

    let action_row = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .align_center()
        .margin_bottom(20)
        .child(android_btn)
        .child(ios_btn)
        .child(clear_btn);

    let source_line = match &state.source_file {
        Some(path) => format!("来源: {}", path),
        None => "尚未加载文件".to_string(),
    };

    let status_text = ui::Element::new(ui::ElementType::P, Some(state.status.as_str()))
        .size(14)
        .text_color(status_text_color(state.status.as_str()));
    let source_text = ui::Element::new(ui::ElementType::P, Some(source_line.as_str()))
        .margin_top(6)
        .size(12)
        .text_color(COLOR_TEXT_MUTED);

    let status_block = ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .bg(COLOR_BG_CARD) // 给状态框加上卡片底色
        .border(1, COLOR_BORDER_SOFT)
        .radius(10)
        .padding(16)
        .margin_bottom(24)
        .child(status_text)
        .child(source_text);

    let count_label = format!("{} 台设备", state.devices.len());
    let result_title = ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .align_center()
        .margin_bottom(16)
        .child(
            ui::Element::new(ui::ElementType::P, Some("解析结果"))
                .size(16)
                .text_color(COLOR_TEXT_PRIMARY),
        )
        .child(
            ui::Element::new(ui::ElementType::Span, Some(count_label.as_str()))
                .margin_left(10)
                .padding_top(2)
                .padding_bottom(2)
                .padding_left(8)
                .padding_right(8)
                .size(11)
                .text_color(COLOR_TEXT_SECONDARY)
                .bg(COLOR_BG_BADGE)
                .radius(6),
        );

    let result_list = build_device_list(state);

    ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .child(action_row)
        .child(status_block)
        .child(result_title)
        .child(result_list)
}

fn build_device_list(state: &UiState) -> ui::Element {
    if state.devices.is_empty() {
        return ui::Element::new(ui::ElementType::Div, None)
            .width_full()
            .padding(30)
            .flex()
            .justify_center()
            .align_center()
            .bg(COLOR_BG_CARD)
            .border(1, COLOR_BORDER_SOFT)
            .radius(10)
            .child(
                ui::Element::new(ui::ElementType::P, Some("等待提取数据..."))
                    .size(13)
                    .text_color(COLOR_TEXT_MUTED),
            );
    }

    let mut container = ui::Element::new(ui::ElementType::Div, None).width_full();
    for (index, item) in state.devices.iter().enumerate() {
        let lock_event = format!("{}{}", KEY_INPUT_LOCK_PREFIX, index);

        let platform_badge =
            ui::Element::new(ui::ElementType::Span, Some(item.platform.as_label()))
                .padding_top(3)
                .padding_bottom(3)
                .padding_left(8)
                .padding_right(8)
                .size(11)
                .text_color(COLOR_TEXT_SECONDARY)
                .bg(COLOR_BG_BADGE)
                .radius(6);

        let card_title = ui::Element::new(ui::ElementType::P, Some(item.name.as_str()))
            .size(15)
            .margin_left(10)
            .text_color(COLOR_TEXT_PRIMARY);

        let header_row = ui::Element::new(ui::ElementType::Div, None)
            .width_full()
            .flex()
            .flex_direction(ui::FlexDirection::Row)
            .align_center()
            .child(platform_badge)
            .child(card_title);

        let card = ui::Element::new(ui::ElementType::Div, None)
            .width_full()
            .margin_bottom(14)
            .bg(COLOR_BG_CARD) // 设备列表使用卡片设计
            .border(1, COLOR_BORDER_SOFT)
            .radius(10)
            .padding(16)
            .transition("all 0.2s ease")
            .child(header_row)
            .child(
                ui::Element::new(ui::ElementType::P, Some("encryptKey"))
                    .margin_top(14)
                    .size(12)
                    .text_color(COLOR_TEXT_MUTED),
            )
            .child(
                ui::Element::new(ui::ElementType::Input, Some(item.encrypt_key.as_str()))
                    .without_default_styles()
                    .margin_top(8)
                    .width_full()
                    .padding(12)
                    .bg(COLOR_BG_INPUT)
                    .border(1, COLOR_BORDER_SOFT)
                    .radius(8)
                    .size(13)
                    .text_color(COLOR_TEXT_PRIMARY)
                    .transition("all 0.2s ease")
                    .on(ui::Event::Input, lock_event.as_str()),
            );

        container = container.child(card);
    }
    container
}

fn build_tutorial_video(video_src: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::Video, Some(video_src))
        .width_full()
        .height(260)
        .margin_top(16)
        .border(1, COLOR_BORDER_SOFT)
        .radius(10)
}

fn build_tutorial_section(title: &str, lines: &[&str], video_src: &str) -> ui::Element {
    let section_title = ui::Element::new(ui::ElementType::P, Some(title))
        .size(16)
        .text_color(COLOR_TEXT_PRIMARY);

    let mut list = ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .margin_top(12)
        .bg(COLOR_BG_CARD) // 卡片化
        .border(1, COLOR_BORDER_SOFT)
        .radius(10)
        .padding(16);

    for (index, line) in lines.iter().enumerate() {
        let line_text = format!("{}. {}", index + 1, line);
        let mut el = ui::Element::new(ui::ElementType::P, Some(line_text.as_str()))
            .size(13)
            .text_color(COLOR_TEXT_SECONDARY);
        if index > 0 {
            el = el.margin_top(10);
        }
        list = list.child(el);
    }

    ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .margin_bottom(24)
        .child(section_title)
        .child(list)
        .child(build_tutorial_video(video_src))
}

fn build_tutorial_tab() -> ui::Element {
    let android_section = build_tutorial_section(
        "Android 导出流程",
        &[
            "打开小米运动健康App，点击底栏中的“我的”，滑动到底部，进入“关于”页",
            "在页面中快速连续点击App图标多次，直到出现“Log迁移至...”提示，点击确定",
            "在该插件功能页中点击“选择Android ZIP”，进入路径“本机内部存储 > Downloads > wearablelog”，选取最新的log压缩包",
        ],
        TUTORIAL_VID_ANDROID,
    );
    let ios_section = build_tutorial_section(
        "iOS 导出流程",
        &[
            "在该插件功能页中点击“选择iOS SQLite”",
            "在打开的文件选取器中进入“我的iPhone”",
            "进入路径“小米运动健康 > MHWCahe > 你的小米账号ID > VirtualDevice_registerList”",
            "选取名为“manifest.sqlite”的文件"
        ],
        TUTORIAL_VID_IOS,
    );

    ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .child(android_section)
        .child(ios_section)
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
