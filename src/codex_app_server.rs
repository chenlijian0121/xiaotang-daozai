use super::{parse_codex_app_limits, Snapshot};
use serde_json::{json, Value};
use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

/// 在标准安装目录定位原生 CLI；不执行当前目录中的同名文件、npm 脚本或 shell。
fn executable() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    let profile = env::var_os("USERPROFILE").map(PathBuf::from).filter(|p| p.is_absolute());
    // 部分启动器没有继承 APPDATA；此时核对用户目录下 npm 的标准安装位置。
    let roaming = env::var_os("APPDATA").map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| profile.as_ref().map(|p| p.join("AppData/Roaming")));
    if let Some(roaming) = roaming {
        let packages = roaming.join("npm/node_modules/@openai");
        let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
        let triple = if cfg!(target_arch = "aarch64") { "aarch64-pc-windows-msvc" } else { "x86_64-pc-windows-msvc" };
        let platform = format!("codex-win32-{arch}");
        for package in [packages.join("codex/node_modules/@openai").join(&platform), packages.join(platform), packages.join("codex")] {
            let vendor = package.join("vendor").join(triple);
            candidates.push(vendor.join("bin/codex.exe"));
            candidates.push(vendor.join("codex/codex.exe"));
        }
    }
    if let Some(profile) = profile {
        candidates.push(profile.join(".codex/bin/codex.exe"));
        candidates.push(profile.join(".local/bin/codex.exe"));
    }
    // 商店版的包目录带版本号，仅接受 Program Files 下 OpenAI Codex 包的资源目录。
    if let (Some(programs), Some(search)) = (env::var_os("ProgramFiles"), env::var_os("PATH")) {
        let packages = PathBuf::from(programs).join("WindowsApps");
        for dir in env::split_paths(&search) {
            let Some(app) = dir.parent() else { continue };
            let Some(package) = app.parent() else { continue };
            if dir.is_absolute()
                && dir.ends_with("resources")
                && app.ends_with("app")
                && package.file_name().is_some_and(|name| name.to_string_lossy().to_ascii_lowercase().starts_with("openai.codex_"))
                && package.parent().is_some_and(|p| p.as_os_str().eq_ignore_ascii_case(packages.as_os_str()))
            {
                candidates.push(dir.join("codex.exe"));
            }
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// 供账号添加与发现判断本机是否已安装可调用的原生 Codex。
pub(super) fn available() -> bool {
    executable().is_some()
}

type QuotaError = (String, u64);

/// 向子进程输入一条 JSON-RPC 消息；刷新管道，避免服务端等待缓冲内容。
fn send(input: &mut impl Write, request: Value) -> Result<(), QuotaError> {
    writeln!(input, "{request}").and_then(|_| input.flush())
        .map_err(|_| ("无法向 Codex 客户端发送额度请求".into(), 30))
}

/// 按请求编号取结果，忽略异步通知；限制响应总字节数，错误信息只返回本地固定文案。
fn result_for(output: &mut impl BufRead, id: u64) -> Result<Value, QuotaError> {
    let mut remaining = 1024 * 1024u64;
    let mut line = Vec::new();
    while remaining > 0 {
        line.clear();
        let count = output.by_ref().take(remaining).read_until(b'\n', &mut line)
            .map_err(|_| ("无法读取 Codex 客户端响应".into(), 30))?;
        if count == 0 {
            return Err(("Codex 客户端已退出，未返回额度".into(), 30));
        }
        remaining -= count as u64;
        if line.last() != Some(&b'\n') {
            break;
        }
        let mut reply: Value = serde_json::from_slice(&line)
            .map_err(|_| ("Codex 客户端响应格式无效".into(), 60))?;
        if reply["id"].as_u64() != Some(id) { continue; }
        if let Some(result) = reply.get_mut("result") { return Ok(result.take()); }
        if reply.pointer("/error/code").and_then(Value::as_i64) == Some(-32601) {
            return Err(("当前 Codex 客户端不支持额度查询，请更新 Codex".into(), 60));
        }
        return Err(("Codex 客户端查询失败，请检查 Codex 登录状态及网络连接".into(), 60));
    }
    Err(("Codex 客户端响应过长或不完整".into(), 60))
}

/// 初始化官方协议并查询当前账户额度；整个会话只发送这三种消息。
fn exchange(mut input: impl Write, mut output: impl BufRead) -> Result<Snapshot, QuotaError> {
    send(&mut input, json!({"id":1,"method":"initialize","params":{
        "clientInfo":{"name":"daozai_quota","version":env!("CARGO_PKG_VERSION")}
    }}))?;
    result_for(&mut output, 1)?;
    send(&mut input, json!({"method":"initialized","params":{}}))?;
    send(&mut input, json!({"id":2,"method":"account/rateLimits/read"}))?;
    let result = result_for(&mut output, 2)?;
    parse_codex_app_limits(&result).map_err(|message| (message, 60))
}

/// 使用 home 对应的 Codex 登录态获取额度。子进程持有认证材料，岛仔只接收额度结果。
/// 握手和查询共用 20 秒超时。查询结束后回收子进程，错误中的秒数供后台服务安排重试。
pub(super) fn fetch(home: &Path) -> Result<Snapshot, QuotaError> {
    let exe = executable().ok_or_else(|| ("未找到本机 Codex 客户端".to_owned(), 60))?;
    let mut command = Command::new(&exe);
    command.args(["app-server", "--listen", "stdio://"])
        .env("CODEX_HOME", home)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    if let Some(directory) = exe.parent() { command.current_dir(directory); }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW，后台刷新不弹出控制台。
    }
    let mut child = command.spawn().map_err(|_| ("无法启动本机 Codex 客户端".to_owned(), 60))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let pipes = child.stdin.take().zip(child.stdout.take());
    let result = match pipes.and_then(|(input, output)| {
        thread::Builder::new().name("codex-quota-stdio".into()).spawn(move || {
            let _ = sender.send(exchange(input, BufReader::new(output)));
        }).ok()
    }) {
        Some(_) => receiver.recv_timeout(Duration::from_secs(20)).unwrap_or_else(|e| match e {
            mpsc::RecvTimeoutError::Timeout => Err(("Codex 额度查询超时，请稍后刷新".into(), 30)),
            mpsc::RecvTimeoutError::Disconnected => Err(("Codex 客户端通信中断".into(), 30)),
        }),
        None => Err(("无法创建 Codex 客户端通信通道".into(), 30)),
    };
    // 成功响应也结束本次专用进程，避免刷新后留下空闲服务。
    if child.try_wait().ok().flatten().is_none() { let _ = child.kill(); }
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 初始化通知可与额度回复交错，匹配请求编号后才能消费该结果。
    #[test]
    fn ignores_notifications_and_other_request_ids() {
        let messages = b"{\"method\":\"notice\"}\n{\"id\":1,\"result\":null}\n{\"id\":2,\"result\":{\"ok\":true}}\n";
        assert_eq!(result_for(&mut Cursor::new(messages), 2).unwrap(), json!({"ok":true}));
    }

    /// 即便服务端在错误正文附加敏感信息，界面也只显示固定提示。
    #[test]
    fn remote_error_text_stays_out_of_the_ui() {
        let message = b"{\"id\":2,\"error\":{\"message\":\"sensitive fixture\"}}\n";
        let (error, _) = result_for(&mut Cursor::new(message), 2).unwrap_err();
        assert!(!error.contains("sensitive fixture"));
    }

    /// 未换行的大响应也必须及时停止读取，避免刷新线程无限分配内存。
    #[test]
    fn rejects_an_unbounded_frame() {
        let message = vec![b'x'; 1024 * 1024 + 1];
        assert!(result_for(&mut Cursor::new(message), 2).unwrap_err().0.contains("过长"));
    }
}
