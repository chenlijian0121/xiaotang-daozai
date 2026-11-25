//! 管理用户主动连接的 Antigravity CLI 状态行，只修改岛仔自己的采集命令。
use serde_json::{json, Value};
use std::{env, fs, io::ErrorKind, path::{Path, PathBuf}, sync::Mutex};

const PREFIX: &str = "pushd ";
const SUFFIX: &str = r" && .\daozai.exe --antigravity-statusline";
const BOM: &[u8] = b"\xef\xbb\xbf";
static SETTINGS_WRITE: Mutex<()> = Mutex::new(());

/// 定位当前用户的 CLI 设置；缺少主目录时不写入相对路径。
fn settings_path() -> Result<PathBuf, String> {
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"))
        .map(PathBuf::from).filter(|p| p.is_absolute()).ok_or("无法定位 Antigravity 用户目录")?;
    Ok(home.join(".gemini").join("antigravity-cli").join("settings.json"))
}

/// cmd 扩展模式下 pushd 能保留目录中的空格，&& 保证切换失败时不执行当前目录的程序。
/// 目录必须是普通绝对路径，拒绝环境变量或 shell 运算符，避免命令展开改变执行目标。
fn safe_directory(dir: &str) -> bool {
    let bytes = dir.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":\\";
    let network = dir.starts_with(r"\\") && !dir.starts_with(r"\\.\") && !dir.starts_with(r"\\?\");
    (drive || network) && !dir.chars().any(|c| c.is_control() || "\"&|<>^%!".contains(c))
}

/// 将当前原生程序的路径转为 CLI 的 cmd /c 命令，兼容带空格的路径及 Windows 长路径前缀。
fn collector_command(exe: &Path) -> Result<String, String> {
    if !exe.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("daozai.exe")) {
        return Err("请使用 daozai.exe 连接 Antigravity 状态行".into());
    }
    let dir = exe.parent().and_then(Path::to_str).ok_or("无法识别岛仔程序目录")?.replace('/', "\\");
    let dir = if let Some(unc) = dir.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        dir.strip_prefix(r"\\?\").unwrap_or(&dir).to_owned()
    };
    if !safe_directory(&dir) {
        return Err("岛仔安装路径含有 cmd 无法安全展开的字符，无法注册 Antigravity 状态行".into());
    }
    Ok(format!("{PREFIX}{dir}{SUFFIX}"))
}

/// 只接受本模块生成的固定命令；安装目录变化后仍可识别原命令，用户追加的参数会保留。
fn owned_command(command: &str) -> bool {
    command.strip_prefix(PREFIX).and_then(|s| s.strip_suffix(SUFFIX)).is_some_and(safe_directory)
}

/// 缺少文件表示首次配置；读取或解析失败时不可用默认值覆盖原文件。
fn read_settings(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err("无法读取 Antigravity 的 settings.json".into()),
    }
}

/// 加入或移除采集命令。返回 true 只表示本次新增了注册，供账号保存失败时回滚。
fn update(path: &Path, install_command: Option<&str>) -> Result<bool, String> {
    let _guard = SETTINGS_WRITE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let original = read_settings(path)?;
    let bytes = original.as_deref().unwrap_or(b"{}");
    let mut settings: Value = serde_json::from_slice(bytes.strip_prefix(BOM).unwrap_or(bytes))
        .map_err(|_| "Antigravity 的 settings.json 不是有效 JSON，已保留原文件")?;
    let fields = settings.as_object_mut().ok_or("Antigravity 的 settings.json 顶层必须是对象")?;
    let status = fields.get("statusLine");
    let previous = status.and_then(|entry| entry.get("command")).and_then(Value::as_str);
    let owned = previous.is_some_and(owned_command);
    let mut added = false;
    if let Some(command) = install_command {
        if previous == Some(command) && status.is_some_and(|v| v["enabled"] != false) {
            return Ok(false);
        }
        if owned {
            let line = &mut fields["statusLine"];
            line["command"] = json!(command);
            if line["enabled"] == false { line["enabled"] = json!(true); }
        } else {
            // 解除连接后用户自定的 padding 等显示选项仍可沿用；显式禁用或其他类型则保留。
            if status.is_some_and(|s| !s.is_null() && !(s.is_object()
                && s.get("command").is_none() && s.get("type").is_none()
                && s.get("stack_with_default").is_none() && s["enabled"] != false)) {
                return Err("Antigravity 已配置其他状态行，已保留原设置；如需连接，请先在 CLI 中处理现有状态行".into());
            }
            let line = fields.entry("statusLine").or_insert(Value::Null);
            line["type"] = json!("command");
            line["command"] = json!(command);
            line["stack_with_default"] = json!(true);
            added = true;
        }
    } else if owned {
        // 用户可能在安装后调整 padding 等设置；仅移除属于采集器的字段。
        let line = fields.get_mut("statusLine").and_then(Value::as_object_mut).unwrap();
        line.remove("command");
        if line.get("type").and_then(Value::as_str) == Some("command") { line.remove("type"); }
        if line.get("stack_with_default").and_then(Value::as_bool) == Some(true) { line.remove("stack_with_default"); }
        if line.is_empty() { fields.remove("statusLine"); }
    } else {
        return Ok(false);
    }
    let mut output = if bytes.starts_with(BOM) { BOM.to_vec() } else { Vec::new() };
    serde_json::to_writer_pretty(&mut output, &settings).map_err(|_| "无法编码 Antigravity 设置")?;
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).map_err(|_| "无法创建 Antigravity 设置目录")?; }
    // 防止本次解析期间 CLI 或其他编辑器刚保存的改动被旧快照覆盖。
    if read_settings(path)? != original { return Err("Antigravity 设置已发生变化，请重试".into()); }
    crate::model::atomic_write(path, &output).map_err(|_| "无法保存 Antigravity 状态行设置")?;
    Ok(added)
}

/// 显式连接时注册当前岛仔；返回是否新注册，以便调用方在后续保存失败时撤销。
pub(super) fn install() -> Result<bool, String> {
    let exe = env::current_exe().map_err(|_| "无法定位岛仔程序")?;
    update(&settings_path()?, Some(&collector_command(&exe)?))
}

/// 用户移除来源时只撤销岛仔拥有的命令；自定义命令和缺失的配置均视作已解除。
pub(super) fn uninstall() -> Result<(), String> {
    update(&settings_path()?, None).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 为每个并行测试提供独立的设置文件，绝不使用真实 CLI 用户目录。
    fn fixture() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        env::temp_dir().join(format!("daozai-agy-settings-{}-{}-{}", std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(), N.fetch_add(1, Ordering::Relaxed)))
            .join("settings.json")
    }

    #[test]
    fn registration_preserves_foreign_fields_and_handles_relocation() {
        let path = fixture();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"future":{"n":42}}"#).unwrap();
        let cmd = format!("{PREFIX}C:\\Program Files\\Old{SUFFIX}");
        let moved = format!("{PREFIX}D:\\Program Files (x86)\\New{SUFFIX}");
        assert!(update(&path, Some(&cmd)).unwrap());
        let before = fs::read(&path).unwrap();
        assert!(!update(&path, Some(&cmd)).unwrap());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!update(&path, Some(&moved)).unwrap());
        let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(document["statusLine"]["command"], moved);
        assert_eq!(document["statusLine"]["stack_with_default"], true);
        document["statusLine"]["padding"] = json!(2);
        fs::write(&path, document.to_string()).unwrap();
        update(&path, None).unwrap();
        let after: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after, json!({"future":{"n":42},"statusLine":{"padding":2}}));
        update(&path, None).unwrap();
        assert!(update(&path, Some(&moved)).unwrap());
        let restored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored["statusLine"]["padding"], 2);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn foreign_or_invalid_settings_are_never_overwritten() {
        let path = fixture();
        let cmd = format!("{PREFIX}C:\\Apps\\Daozai{SUFFIX}");
        update(&path, None).unwrap();
        assert!(!path.exists());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for raw in [b"invalid".as_slice(), b"[]", br#"{"statusLine":{"command":"my-tool"}}"#] {
            fs::write(&path, raw).unwrap();
            assert!(update(&path, Some(&cmd)).is_err());
            let _ = update(&path, None);
            assert_eq!(fs::read(&path).unwrap(), raw);
        }
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_allow_spaces_but_reject_shell_expansion() {
        let cmd = collector_command(Path::new(r"\\?\C:\Program Files (x86)\岛仔\daozai.exe")).unwrap();
        assert_eq!(cmd, r"pushd C:\Program Files (x86)\岛仔 && .\daozai.exe --antigravity-statusline");
        assert!(owned_command(&cmd));
        assert!(!owned_command(&format!("{cmd} & another-tool")));
        for dir in ["A&B", "%USERPROFILE%", "A!B!", "A^B"] {
            let exe = PathBuf::from("C:\\").join(dir).join("daozai.exe");
            assert!(collector_command(&exe).is_err());
        }
    }
}
