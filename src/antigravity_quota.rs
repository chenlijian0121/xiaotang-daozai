use super::{cadence, reset, Snapshot, Window};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{env, fs, io::Read, path::{Path, PathBuf}};

/// 定位 Google 安装器在 Windows 上使用的用户级安装目录，不从工作目录查找同名程序。
fn executable() -> Option<PathBuf> {
    [("USERPROFILE", ".gemini/bin/agy.exe"), ("LOCALAPPDATA", "agy/bin/agy.exe")]
        .into_iter().filter_map(|(key, suffix)| {
            let base = PathBuf::from(env::var_os(key)?);
            base.is_absolute().then(|| base.join(suffix))
        }).find(|p| p.is_file())
}

/// 返回当前用户是否已安装 Antigravity CLI；登录状态由官方客户端自行管理。
pub(super) fn available() -> bool { executable().is_some() }

/// 读取已启用的 Windows 手动代理及例外列表；按协议设置时只选 HTTPS 项。
#[cfg(windows)]
fn system_https_proxy() -> Option<(String, String)> {
    use windows::{core::{w, PCWSTR}, Win32::{Foundation::ERROR_FILE_NOT_FOUND,
        System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ}}};
    let key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
    let mut enabled = 0u32;
    let mut size = std::mem::size_of_val(&enabled) as u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, key, w!("ProxyEnable"), RRF_RT_REG_DWORD, None,
        Some((&mut enabled as *mut u32).cast()), Some(&mut size)).ok().ok()?; }
    if enabled == 0 { return None; }
    let read = |name: PCWSTR| -> Option<String> {
        let mut bytes = 0;
        let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, key, name, RRF_RT_REG_SZ, None, None, Some(&mut bytes)) };
        if status == ERROR_FILE_NOT_FOUND { return Some(String::new()); }
        status.ok().ok()?;
        if bytes > 128 * 1024 { return None; }
        let mut raw = vec![0u16; (bytes as usize).div_ceil(2)];
        unsafe { RegGetValueW(HKEY_CURRENT_USER, key, name, RRF_RT_REG_SZ, None,
            Some(raw.as_mut_ptr().cast()), Some(&mut bytes)).ok().ok()?; }
        raw.truncate(raw.iter().position(|c| *c == 0).unwrap_or(raw.len()));
        String::from_utf16(&raw).ok()
    };
    let server = read(w!("ProxyServer"))?;
    let proxy = https_from_setting(&server)?;
    // 未配置例外表示空列表；读取出错时不应丢弃未知例外并强行开启代理。
    Some((proxy, read(w!("ProxyOverride"))?))
}

/// 裸 host:port 表示 HTTP 转发代理；显式协议与地址必须是 Go 支持的代理 URL。
fn proxy_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() { return None; }
    let url = if value.contains("://") { value.to_owned() } else { format!("http://{value}") };
    let parsed = url::Url::parse(&url).ok()?;
    (parsed.host().is_some() && matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h")).then_some(url)
}

/// 单一代理适用于 HTTPS；分协议配置缺少 HTTPS 时保持直连，绝不挪用 HTTP 专用代理。
fn https_from_setting(raw: &str) -> Option<String> {
    let entries: Vec<_> = raw.split(';').map(str::trim).collect();
    if entries.iter().any(|e| e.split_once('=').is_some_and(|(k, _)| !k.trim().is_empty() && k.trim().bytes().all(|b| b.is_ascii_alphabetic()))) {
        return entries.iter().find_map(|e| {
            let (proto, server) = e.split_once('=')?;
            proto.trim().eq_ignore_ascii_case("https").then(|| proxy_url(server)).flatten()
        });
    }
    proxy_url(raw)
}

/// 将 Windows IP 通配例外转换为 Go 可识别的前缀，部分八位组如 172.2* 按十进制前缀展开。
fn ip_exceptions(pattern: &str) -> Vec<String> {
    let mut parts: Vec<_> = pattern.split('.').collect();
    let Some(last) = parts.pop() else { return Vec::new() };
    if parts.is_empty() || parts.len() > 3 || !last.bytes().all(|c| c.is_ascii_digit()) { return Vec::new(); }
    let mut ip = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        let Ok(n) = part.parse::<u8>() else { return Vec::new() };
        if n.to_string() != *part { return Vec::new(); }
        ip[i] = n;
    }
    if last.is_empty() { return vec![format!("{}/{}", std::net::Ipv4Addr::from(ip), parts.len() * 8)]; }
    (0u8..=255).filter(|n| n.to_string().starts_with(last)).map(|n| {
        ip[parts.len()] = n;
        format!("{}/{}", std::net::Ipv4Addr::from(ip), (parts.len() + 1) * 8)
    }).collect()
}

/// 转换成 NO_PROXY 仅保留系统已允许的直连范围。Go 无法匹配包含子串或任意无点主机的例外。
fn go_exceptions(raw: &str) -> Vec<String> {
    let mut rules = Vec::new();
    for pattern in raw.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        if pattern == "*" || pattern.eq_ignore_ascii_case("localhost") || pattern.parse::<std::net::IpAddr>().is_ok() {
            rules.push(pattern.to_owned());
        } else if let Some(prefix) = pattern.strip_suffix('*') {
            rules.extend(ip_exceptions(prefix));
        } else if let Some(suffix) = pattern.strip_prefix('*') {
            // *.domain 对应 Go 的 .domain；*domain 同时包括根域，只有跨标签前缀无法表达。
            if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')) {
                rules.push(suffix.to_owned());
            }
        }
        // Windows 的精确 DNS 名称在 Go 中会扩大为包含所有子域名，保持代理比扩展直连更稳妥。
    }
    rules
}

/// 仅为即将启动的 agy 构造 UTF-16 环境块；没有新增代理时返回 None 并继承原环境。
#[cfg(windows)]
fn proxy_environment(mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    system: impl FnOnce() -> Option<(String, String)>) -> Option<Vec<u16>> {
    use std::{ffi::{OsStr, OsString}, os::windows::ffi::OsStrExt};
    let current = |name: &str| vars.iter().find(|(key, _)| key.eq_ignore_ascii_case(OsStr::new(name))).map(|(_, value)| value.as_os_str());
    if current("HTTPS_PROXY").is_some_and(|v| !v.is_empty()) { return None; }
    let generic = current("ALL_PROXY").and_then(OsStr::to_str).and_then(proxy_url);
    let (proxy, exceptions) = generic.map(|url| (url, String::new())).or_else(system)?;
    let mut no_proxy = current("NO_PROXY").unwrap_or_default().to_owned();
    let additions = go_exceptions(&exceptions);
    if !additions.is_empty() {
        if !no_proxy.is_empty() { no_proxy.push(","); }
        no_proxy.push(additions.join(","));
        vars.retain(|(k, _)| !k.eq_ignore_ascii_case(OsStr::new("NO_PROXY")));
        vars.push((OsString::from("NO_PROXY"), no_proxy));
    }
    vars.retain(|(k, _)| !k.eq_ignore_ascii_case(OsStr::new("HTTPS_PROXY")));
    vars.push((OsString::from("HTTPS_PROXY"), OsString::from(proxy)));
    // 环境块按变量名排序，保留所有原始 UTF-16 值、盘符隐藏项以及值内的等号；空项标记结尾。
    vars.sort_by_cached_key(|(key, _)| key.to_string_lossy().to_uppercase().encode_utf16().collect::<Vec<_>>());
    let mut block = Vec::new();
    for (key, value) in vars {
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    Some(block)
}

/// 仅在用户点击打开时创建带独立控制台的官方 CLI，不传提示词、模型请求或认证参数。
pub(super) fn open_cli() -> Result<(), String> {
    let exe = executable().ok_or("未找到 Antigravity CLI，请先安装官方客户端")?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{core::{PCWSTR, PWSTR}, Win32::{Foundation::CloseHandle, System::Threading::{
            CreateProcessW, CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
        }}};
        let wide: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut args: Vec<u16> = Some(b'"' as u16).into_iter().chain(exe.as_os_str().encode_wide())
            .chain([b'"' as u16, 0]).collect();
        let user = env::var_os("USERPROFILE").map(PathBuf::from).filter(|p| p.is_absolute());
        let cwd = user.as_ref().map(|p| p.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>());
        let environment = proxy_environment(env::vars_os().collect(), system_https_proxy);
        let startup = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
        let mut info = PROCESS_INFORMATION::default();
        // 不设置 STARTF_USESTDHANDLES，不继承 GUI 的空句柄；Windows 为新控制台提供真实输入输出。
        unsafe {
            CreateProcessW(PCWSTR(wide.as_ptr()), PWSTR(args.as_mut_ptr()), None, None, false,
                CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT, environment.as_ref().map(|block| block.as_ptr().cast()),
                cwd.as_ref().map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
                &startup, &mut info).map_err(|_| "无法打开 Antigravity CLI，请检查安装目录")?;
            let _ = CloseHandle(info.hThread);
            let _ = CloseHandle(info.hProcess);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(exe).spawn().map(|_| ()).map_err(|_| "无法打开 Antigravity CLI".into())
    }
}

/// 从 Antigravity 明确返回的模型额度桶解析窗口。remaining_fraction 的单位是 0–1；
/// 剩余点数缺少总量，禁用或未提供百分比的窗口均无法换算。
fn quota_window(bucket: &Value, seconds: i64, at: i64) -> Option<Window> {
    if bucket["disabled"].as_bool() == Some(true) { return None; }
    let fraction = bucket["remaining_fraction"].as_f64()?;
    if !(0.0..=1.0).contains(&fraction) { return None; }
    let reset = reset(&bucket["reset_time"]).or_else(|| {
        let delta = bucket["reset_in_seconds"].as_i64().filter(|d| *d >= 0)?;
        at.checked_add(delta)
    });
    Some(Window { label: cadence(Some(seconds)), seconds: Some(seconds),
        remaining: (fraction * 100.0) as f32, reset, amount: None })
}

/// 从官方状态栏事件提取 Gemini 共用的五小时、每周额度；at 为事件采集的 Unix 秒数，
/// 用于定位相对重置时间。某个周期缺失时只保留实际返回的另一个周期。
pub(super) fn parse_statusline(payload: &Value, at: i64) -> Result<Snapshot, String> {
    let quota = &payload["quota"];
    let windows: Vec<_> = [("gemini-5h", 18000), ("gemini-weekly", 604800)].into_iter()
        .filter_map(|(id, duration)| quota_window(&quota[id], duration, at)).collect();
    if windows.is_empty() { return Err("Antigravity 尚未提供 Gemini 的五小时或每周额度".into()); }
    // 仅保留已知公开套餐名称，避免将输入中的其他文本写入本地快照。
    let plan = ["Free", "Plus", "Pro", "Ultra", "Enterprise"].into_iter().find(|p|
        payload["plan_tier"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(p))).unwrap_or_default();
    Ok(Snapshot { plan: plan.to_owned(),
        at, windows, ..Default::default() })
}

/// 只保存本机最近一次状态栏事件的额度投影。等待上报或登录中都用空窗口覆盖旧账号。
#[derive(Serialize, Deserialize)]
struct Capture {
    observed_ms: i64,
    authenticating: bool,
    owner_digest: Option<String>,
    plan: String,
    windows: Vec<Window>,
}

/// 所有额度与登录状态都写入岛仔当前数据目录下的同一个缓存。
fn snapshot_path() -> PathBuf { crate::model::config_dir().join("antigravity-quota.json") }

/// 最后一张 Antigravity 卡片移除后清空旧快照。保留清空时刻以挡住尚在处理的旧事件。
pub(super) fn clear_snapshot() -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    let path = snapshot_path();
    if !path.parent().is_some_and(Path::is_dir) { return Ok(()); }
    let mut lock = snapshot_lock(&path)?;
    lock.seek(SeekFrom::Start(0)).and_then(|_| lock.write_all(&chrono::Utc::now().timestamp_millis().to_le_bytes()))
        .map_err(|_| "无法记录 Antigravity 缓存清理时间")?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("无法清理 Antigravity 额度快照".into()),
    }
}

/// 所有写入与清理使用同一跨进程锁；进程退出时操作系统自动释放该锁。
fn snapshot_lock(path: &Path) -> Result<fs::File, String> {
    let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(path.with_extension("lock")).map_err(|_| "无法打开 Antigravity 缓存锁")?;
    lock.lock().map_err(|_| "无法锁定 Antigravity 缓存")?;
    Ok(lock)
}

/// 为标准输入及缓存设置字节上限；超限、截断或非 JSON 输入不会作为有效事件落盘。
fn bounded_json<T: serde::de::DeserializeOwned>(input: impl Read, limit: u64) -> Result<T, String> {
    let mut bytes = Vec::new();
    input.take(limit + 1).read_to_end(&mut bytes).map_err(|_| "无法读取 Antigravity 额度状态")?;
    if bytes.len() as u64 > limit { return Err("Antigravity 状态数据过大".into()); }
    serde_json::from_slice(&bytes).map_err(|_| "Antigravity 状态格式无效".into())
}

/// 返回上次官方状态事件对应的快照；刷新时沿用采集时间，不把本地读取当作新上报。
pub(super) fn fetch() -> Result<Snapshot, (String, u64)> {
    let input = fs::File::open(snapshot_path()).map_err(|e| {
        let message = if e.kind() == std::io::ErrorKind::NotFound {
            "等待 Antigravity 状态栏上报，请打开官方 CLI 并登录"
        } else { "无法读取 Antigravity 本地额度状态" };
        (message.to_owned(), 0)
    })?;
    let data: Capture = bounded_json(input, 32 * 1024).map_err(|e| (e, 0))?;
    if data.authenticating { return Err(("请在 Antigravity CLI 完成登录后刷新".into(), 0)); }
    if data.windows.is_empty() { return Err(("Antigravity 尚未提供 Gemini 的五小时或每周额度".into(), 0)); }
    Ok(Snapshot { at: data.observed_ms / 1000, plan: data.plan, windows: data.windows, ..Default::default() })
}

/// 读取一次官方状态栏 JSON，仅落盘白名单字段。返回成功时无需向状态栏打印任何文本。
pub(super) fn collect(input: impl Read) -> Result<(), String> {
    let observed_ms = chrono::Utc::now().timestamp_millis();
    let payload: Value = bounded_json(input, 1024 * 1024)?;
    store_event(&snapshot_path(), &payload, observed_ms)
}

/// 使用同目录文件锁按收到事件的先后顺序写入，避免多个终端同时更新造成旧记录回写。
/// 相同额度最多每分钟更新一次采集时间；账号切换时从新事件重新投影，绝不填充旧窗口。
fn store_event(path: &Path, payload: &Value, observed_ms: i64) -> Result<(), String> {
    if payload["product"].as_str() != Some("antigravity") { return Err("输入并非 Antigravity 状态栏事件".into()); }
    let authenticating = matches!(payload["agent_state"].as_str(), Some("authenticating" | "signed_out" | "logged_out"));
    let snapshot = if authenticating { None } else { parse_statusline(payload, observed_ms / 1000).ok() };
    // 两个账号即便显示相同数值也应记录新的采集时间。只比较摘要，不保存或显示地址。
    let owner_digest = payload["email"].as_str().map(str::trim).filter(|s| !s.is_empty() && !authenticating)
        .map(|email| ring::digest::digest(&ring::digest::SHA256, email.to_ascii_lowercase().as_bytes())
            .as_ref().iter().map(|b| format!("{b:02x}")).collect());
    let data = Capture {
        observed_ms, authenticating, owner_digest,
        plan: snapshot.as_ref().map(|s| s.plan.clone()).unwrap_or_default(),
        windows: snapshot.map(|s| s.windows).unwrap_or_default(),
    };
    let parent = path.parent().ok_or("Antigravity 缓存目录无效")?;
    fs::create_dir_all(parent).map_err(|_| "无法创建 Antigravity 缓存目录")?;
    let mut lock = snapshot_lock(path)?;
    let mut cleared = [0u8; 8];
    match lock.read_exact(&mut cleared) {
        Ok(()) if observed_ms <= i64::from_le_bytes(cleared) => return Ok(()),
        Err(e) if e.kind() != std::io::ErrorKind::UnexpectedEof => return Err("无法读取 Antigravity 缓存清理时间".into()),
        _ => {}
    }
    if let Some(old) = fs::File::open(path).ok().and_then(|f| bounded_json::<Capture>(f, 32 * 1024).ok()) {
        if old.observed_ms > observed_ms || (observed_ms.saturating_sub(old.observed_ms) < 60_000 &&
            old.authenticating == data.authenticating && old.owner_digest == data.owner_digest &&
            old.plan == data.plan && old.windows == data.windows) {
            return Ok(());
        }
    }
    let raw = serde_json::to_vec(&data).map_err(|_| "无法生成 Antigravity 额度快照")?;
    crate::model::atomic_write(path, &raw).map_err(|_| "无法保存 Antigravity 额度快照".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 显式系统配置只取 HTTPS 目标；HTTP 专用代理、关闭设置时不改变 HTTPS 的直连行为。
    #[test]
    fn https_selection_respects_per_scheme_settings() {
        assert_eq!(https_from_setting("127.0.0.1:8000").as_deref(), Some("http://127.0.0.1:8000"));
        assert_eq!(https_from_setting("http=localhost:80;HTTPS=localhost:8080").as_deref(), Some("http://localhost:8080"));
        assert_eq!(https_from_setting("http=localhost:8000"), None);
        assert_eq!(https_from_setting("ftp://localhost"), None);
    }

    /// 校验例外列表不扩大允许范围，八位组通配不会误作单个子网或模糊域名。
    #[test]
    fn bypass_conversion_preserves_representable_ranges() {
        let rules = go_exceptions("localhost;*.test;*example.invalid;10.*;172.2*;*partial*;<local>;host.exact");
        for rule in ["localhost", ".test", "example.invalid", "10.0.0.0/8",
            "172.2.0.0/16", "172.23.0.0/16", "172.255.0.0/16"] {
            assert!(rules.iter().any(|s| s == rule), "缺少 {rule}");
        }
        for rule in ["172.19.0.0/16", "172.30.0.0/16", "*partial*", "<local>", "host.exact"] {
            assert!(!rules.iter().any(|s| s == rule), "范围不应包含 {rule}");
        }
    }

    /// 只有 agy 子环境补上 HTTPS 代理；原 Path、带等号的值、盘符隐藏项及用户 NO_PROXY 完整继承。
    #[cfg(windows)]
    #[test]
    fn child_block_keeps_user_proxy_precedence_and_unicode_paths() {
        use std::ffi::OsString;
        let vars = |pairs: &[(&str, &str)]| pairs.iter().map(|(k,v)| (OsString::from(k), OsString::from(v))).collect();
        assert!(proxy_environment(vars(&[("hTtPs_PrOxY","http://chosen.test")]), || panic!("显式代理无需注册表")).is_none());
        assert!(proxy_environment(vars(&[("HTTP_PROXY","http://http-only.test")]), || None).is_none());
        let preserved = [("HTTP_PROXY","http://http-only.test"),("no_proxy","corp.test"),("Path","F:\\岛 仔;D:\\bin"),
            ("=F:","F:\\目录=1"),("SAMPLE","a=b")];
        let block = proxy_environment(vars(&preserved), || Some(("http://system.test".into(),"10.*".into()))).unwrap();
        assert_eq!(&block[block.len()-2..], &[0,0]);
        let entries: Vec<_> = block.split(|u| *u == 0).map(String::from_utf16_lossy).collect();
        for item in ["HTTPS_PROXY=http://system.test","HTTP_PROXY=http://http-only.test","NO_PROXY=corp.test,10.0.0.0/8",
            "Path=F:\\岛 仔;D:\\bin","=F:=F:\\目录=1","SAMPLE=a=b"] {
            assert!(entries.iter().any(|s| s == item), "未能继承 {item}");
        }
        let generic = proxy_environment(vars(&[("ALL_PROXY","socks5://127.0.0.1:1080"),("NO_PROXY","internal")]),
            || panic!("有效的 ALL_PROXY 优先")).unwrap();
        let decoded = String::from_utf16(&generic).unwrap();
        assert!(decoded.contains("HTTPS_PROXY=socks5://127.0.0.1:1080\0") && decoded.contains("NO_PROXY=internal\0"));
    }

    /// 两个周期独立换算剩余百分比，绝对重置时间优先于相对秒数。
    #[test]
    fn actual_gemini_buckets_keep_fractions_and_resets() {
        let raw = json!({"quota":{
            "gemini-weekly":{"remaining_fraction":0.625,"reset_time":"2027-01-01T00:00:00Z","reset_in_seconds":5},
            "gemini-5h":{"remaining_fraction":0,"reset_in_seconds":60},
            "3p-weekly":{"remaining_fraction":1.0}}});
        let s = parse_statusline(&raw, 100).unwrap();
        assert_eq!(s.windows.len(), 2);
        assert_eq!((s.windows[0].seconds, s.windows[0].remaining, s.windows[0].reset), (Some(18000), 0.0, Some(160)));
        assert_eq!((s.windows[1].seconds, s.windows[1].remaining, s.windows[1].reset), (Some(604800), 62.5, Some(1798761600)));
    }

    /// 缺失、越界或禁用的周期不构成额度，第三方模型组也不能补充 Gemini。
    #[test]
    fn unavailable_buckets_stay_unknown() {
        for absent in [Value::Null, json!({"remaining_fraction":1.1}), json!({"remaining_amount":77}),
            json!({"disabled":true,"remaining_fraction":1})] {
            let raw = json!({"quota":{"gemini-5h":absent,"gemini-weekly":{"remaining_fraction":1}}});
            let s = parse_statusline(&raw, 100).unwrap();
            assert_eq!(s.windows.len(), 1);
            assert_eq!(s.windows[0].seconds, Some(604800));
        }
        assert!(parse_statusline(&json!({"quota":{"3p-weekly":{"remaining_fraction":1}}}), 100).is_err());
    }

    /// 缓存只投影额度，重复事件节流，切号与登出立即取代旧状态，延迟的旧事件不能复活额度。
    #[test]
    fn cache_follows_account_and_omits_private_session_fields() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("daozai-antigravity-{}-{unique}", std::process::id()));
        let path = dir.join("quota.json");
        let mut raw = json!({"product":"antigravity","email":"first@example.test","cwd":"private-workspace",
            "agent_state":"idle","quota":{"gemini-weekly":{"remaining_fraction":0.5}}});
        store_event(&path, &raw, 100_000).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        assert!(!first.contains("example.test") && !first.contains("private-workspace"));
        store_event(&path, &raw, 101_000).unwrap();
        assert_eq!(first, fs::read_to_string(&path).unwrap());
        raw["email"] = json!("second@example.test");
        store_event(&path, &raw, 102_000).unwrap();
        let switched: Capture = bounded_json(fs::File::open(&path).unwrap(), 32 * 1024).unwrap();
        assert_eq!(switched.observed_ms, 102_000);
        raw["agent_state"] = json!("authenticating");
        store_event(&path, &raw, 103_000).unwrap();
        raw["agent_state"] = json!("idle");
        store_event(&path, &raw, 102_500).unwrap();
        let signed_out: Capture = bounded_json(fs::File::open(&path).unwrap(), 32 * 1024).unwrap();
        assert!(signed_out.authenticating && signed_out.windows.is_empty() && signed_out.owner_digest.is_none());
        fs::remove_file(&path).unwrap();
        fs::remove_file(path.with_extension("lock")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
