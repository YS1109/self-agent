use tauri::{
    menu::{Menu, MenuItemKind},
    AppHandle, Runtime,
};

pub fn chinese_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let menu = Menu::default(app)?;
    localize_items(menu.items()?)?;
    Ok(menu)
}

fn localize_items<R: Runtime>(items: Vec<MenuItemKind<R>>) -> tauri::Result<()> {
    for item in items {
        match item {
            MenuItemKind::Submenu(submenu) => {
                if let Some(label) = chinese_label(&submenu.text()?) {
                    submenu.set_text(label)?;
                }
                localize_items(submenu.items()?)?;
            }
            MenuItemKind::Predefined(item) => {
                if let Some(label) = chinese_label(&item.text()?) {
                    item.set_text(label)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn chinese_label(text: &str) -> Option<&'static str> {
    let normalized = text.replace('&', "");
    Some(match normalized.as_str() {
        "File" => "文件",
        "Edit" => "编辑",
        "View" => "视图",
        "Window" => "窗口",
        "Help" => "帮助",
        "Undo" => "撤销",
        "Redo" => "重做",
        "Cut" => "剪切",
        "Copy" => "复制",
        "Paste" => "粘贴",
        "Select All" => "全选",
        "Minimize" => "最小化",
        "Maximize" | "Zoom" => "缩放",
        "Close" | "Close Window" => "关闭窗口",
        "Fullscreen" | "Full Screen" | "Toggle Full Screen" | "Enter Full Screen" => "切换全屏",
        "Services" => "服务",
        "Hide Others" => "隐藏其他应用",
        "Show All" | "Show All Applications" => "显示所有应用",
        "About" => "关于 Self Agent",
        "Quit" | "Exit" => "退出 Self Agent",
        _ if normalized.starts_with("About ") => "关于 Self Agent",
        _ if normalized.starts_with("Quit ") => "退出 Self Agent",
        _ if normalized.starts_with("Hide ") => "隐藏 Self Agent",
        _ => return None,
    })
}
