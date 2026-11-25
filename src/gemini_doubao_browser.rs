//! 在用户选定的豆包用量页内查询 Gemini 额度。浏览器授权仅在当前进程内复用。
use super::{email, gemini_page, Endpoint, Socket};
use super::super::{parse, request_url, Snapshot, LOGIN_ERROR, PARSE_ERROR};
use serde_json::{json, Value};
use std::{fs, path::Path, sync::atomic::AtomicBool, time::Duration};

const SETUP: &str = "请打开豆包浏览器，在 doubao://inspect/#remote-debugging 开启原生连接";
const PAGE: &str = "请在豆包浏览器打开 gemini.google.com/usage 并登录，再重新连接";
const SWITCHED: &str = "豆包用量页或 Google 账号已切换，请切回原账号并重新连接";

// 请求只在官网用量页执行。返回给岛仔的内容只包含展示额度所需的套餐、用量和重置时间。
const QUERY: &str = r#"async (query) => {
    const usagePage = () => location.origin === 'https://gemini.google.com'
        && /^\/(?:u\/\d+\/)?usage\/?$/.test(location.pathname);
    if (!usagePage()) return {error: 'page'};
    try {
        const response = await fetch(query, {
            mode: 'same-origin', credentials: 'same-origin', cache: 'no-store',
            headers: {Accept: 'application/json'}, signal: AbortSignal.timeout(20000)
        });
        if (response.status === 401 || response.status === 403) return {error: 'login'};
        if (response.status === 429) {
            const retry = response.headers.get('Retry-After')?.trim() || '';
            return {error: 'limit', retryAfter: /^\d+$/.test(retry) ? Math.min(Number(retry), 3600) : null};
        }
        if (!response.ok) return {error: 'network'};
        const finalUrl = new URL(response.url);
        if (finalUrl.origin !== location.origin ||
            !/^\/(?:u\/\d+\/)?_\/BardChatUi\/graphql\/schemas\/GEMINI_WEB_GRAPHQL\/executeQuery$/.test(finalUrl.pathname))
            return {error: 'login'};
        let raw = (await response.text()).trimStart();
        if (raw.startsWith('<')) return {error: 'login'};
        if (raw.length > 1048576) return {error: 'format'};
        if (raw.startsWith(")]}'")) raw = raw.slice(4);
        const payload = JSON.parse(raw);
        const data = Array.isArray(payload) && payload.find(item => item?.data?.viewer !== undefined)?.data;
        if (!data) return {error: 'format'};
        if (!data.viewer) return {error: 'login'};
        const usage = data.viewer.rpcs?.getUsageInfo;
        if (!Array.isArray(usage?.pools)) return {error: 'format'};
        if (!usagePage()) return {error: 'page'};
        const pools = [];
        for (const kind of ['GXU_FIVE_HOURLY', 'GXU_WEEKLY']) {
            const pool = usage.pools.find(p => p?.poolType === kind);
            if (!pool || !Number.isFinite(pool.percentageUsed)) continue;
            const reset = pool.gxu?.windowRefreshesAt;
            const validReset = Number.isSafeInteger(reset) || (typeof reset === 'string'
                && /^\d{4}-\d\d-\d\d[Tt]\d\d:\d\d:\d\d(?:\.\d{1,9})?(?:[Zz]|[+-]\d\d:\d\d)$/.test(reset));
            pools.push({poolType: kind, percentageUsed: pool.percentageUsed,
                gxu: {windowRefreshesAt: validReset ? reset : null}});
        }
        return {subscriptionTier: usage.subscriptionTier === 'TIER1' ? 'TIER1' : '', pools};
    } catch (error) {
        return {error: error instanceof SyntaxError ? 'format' : 'network'};
    }
}"#;

/// 用户明确允许的浏览器连接及其用量标签页。关闭岛仔或豆包后需要重新授权。
pub(in super::super) struct DoubaoConnection {
    socket: Socket,
    page_session: String,
}

/// 从豆包自身的用户目录发现回环端点，兼容浏览器每次启动时分配新的端口。
fn endpoint(root: &Path) -> Result<Endpoint, String> {
    let file = root.join("Doubao/User Data/DevToolsActivePort");
    let raw = fs::read_to_string(file).map_err(|_| SETUP)?;
    Endpoint::parse(&raw).ok_or_else(|| SETUP.into())
}

impl DoubaoConnection {
    /// 在用户点击连接时发现并请求豆包授权，返回连接、已校验的邮箱和首次额度。
    pub(in super::super) fn connect(
        address: &str,
        cancel: &AtomicBool,
    ) -> Result<(Self, String, Snapshot), String> {
        let wanted = if address.trim().is_empty() { None } else { Some(email(address)?) };
        let root = std::env::var_os("LOCALAPPDATA").ok_or(SETUP)?;
        Self::connect_at(&endpoint(Path::new(&root))?, wanted.as_deref(), cancel)
    }

    /// 只在官网用量标签页中选择账号；多个候选页需要用户明确指定邮箱。
    fn connect_at(
        endpoint: &Endpoint,
        wanted: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<(Self, String, Snapshot), String> {
        let mut socket = Socket::open(endpoint, cancel, Duration::from_secs(300))?;
        let targets = socket.call("Target.getTargets", json!({}), None, cancel)?;
        let pages: Vec<_> = targets["targetInfos"].as_array().ok_or(PAGE)?.iter()
            .filter(|item| item["type"] == "page" && item["url"].as_str().is_some_and(gemini_page))
            .filter_map(|item| item["targetId"].as_str())
            .collect();
        if pages.is_empty() { return Err(PAGE.into()); }
        if pages.len() > 1 && wanted.is_none() {
            return Err("豆包中有多个 Gemini 用量页，请填写 Google 邮箱或只保留一个用量页".into());
        }
        for id in pages {
            let attached = socket.call("Target.attachToTarget", json!({"targetId": id, "flatten": true}), None, cancel)?;
            let session = attached["sessionId"].as_str().ok_or(PAGE)?;
            if let Some(current) = socket.identity(session, cancel)?.filter(|addr| wanted.is_none_or(|v| v == addr)) {
                let mut connection = Self { socket, page_session: session.into() };
                let snapshot = connection.read_after_identity(&current, cancel).map_err(|failure| failure.0)?;
                return Ok((connection, current, snapshot));
            }
            // 切换候选页时仅释放调试会话，保留用户已打开的标签页。
            socket.call("Target.detachFromTarget", json!({"sessionId": session}), None, cancel)?;
        }
        Err(if wanted.is_some() { SWITCHED } else { PAGE }.into())
    }

    /// 已核验当前页和邮箱后，读取固定的同源额度接口；数据返回时再次核验，避免串号。
    fn read_after_identity(&mut self, address: &str, cancel: &AtomicBool) -> Result<Snapshot, (String, u64)> {
        let expression = format!("({QUERY})({})", json!(request_url(address)));
        let result = self.socket.call("Runtime.evaluate",
            json!({"expression": expression, "awaitPromise": true, "returnByValue": true}),
            Some(&self.page_session), cancel).map_err(|error| (error, 0))?;
        let current = self.socket.identity(&self.page_session, cancel).map_err(|error| (error, 0))?;
        if current.as_deref() != Some(address) {
            return Err((SWITCHED.into(), 0));
        }
        let usage = result.pointer("/result/value").ok_or_else(|| (PARSE_ERROR.into(), 0))?;
        quota_result(usage)
    }

    /// 后台刷新只沿用已有授权。若标签页关闭或账号变化，向界面返回手动重连提示。
    pub(in super::super) fn refresh(&mut self, address: &str) -> Result<Snapshot, (String, u64)> {
        let cancel = AtomicBool::new(false);
        let current = self.socket.identity(&self.page_session, &cancel).map_err(|error| (error, 0))?;
        if current.as_deref() != Some(address) {
            return Err((SWITCHED.into(), 0));
        }
        self.read_after_identity(address, &cancel)
    }
}

/// 将用量页的结果转成额度快照或调度器的等待时间。限流缺少有效秒数时沿用 Gemini 的三分钟退避。
fn quota_result(usage: &Value) -> Result<Snapshot, (String, u64)> {
    if let Some(reason) = usage["error"].as_str() {
        let (message, delay) = match reason {
            "page" => (PAGE, 0),
            "login" => (LOGIN_ERROR, 0),
            "limit" => ("Gemini 请求过于频繁，请稍后刷新", usage["retryAfter"].as_u64().unwrap_or(180).clamp(30, 3600)),
            "network" => ("豆包浏览器无法查询 Gemini 额度，请检查网络和登录状态后刷新", 0),
            _ => (PARSE_ERROR, 0),
        };
        return Err((message.into(), delay));
    }
    parse(&json!([{"data": {"viewer": {"rpcs": {"getUsageInfo": usage}}}}])).map_err(|error| (error, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::{Ipv4Addr, TcpListener}, thread, time::{SystemTime, UNIX_EPOCH}};
    use tungstenite::Message;

    #[test]
    fn doubao_rate_limit_uses_bounded_retry_after_or_default() {
        for (retry, wait) in [(json!(65), 65), (json!(0), 30), (json!(7200), 3600), (Value::Null, 180)] {
            let (message, delay) = quota_result(&json!({"error":"limit", "retryAfter":retry})).unwrap_err();
            assert!(message.contains("频繁"));
            assert_eq!(delay, wait);
        }
    }

    #[test]
    fn doubao_discovers_its_own_rotating_port() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("daozai-doubao-{}-{stamp}", std::process::id()));
        let profile = root.join("Doubao/User Data");
        fs::create_dir_all(&profile).unwrap();
        assert!(endpoint(&root).is_err());
        let file = profile.join("DevToolsActivePort");
        let route = "/devtools/browser/01234567-89ab-cdef-0123-456789abcdef";
        for port in [9222, 59999] {
            fs::write(&file, format!("{port}\r\n{route}\r\n")).unwrap();
            assert_eq!(endpoint(&root).unwrap().port, port);
        }
        fs::write(&file, "9222\nhttps://example.invalid").unwrap();
        assert!(endpoint(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    /// 仅使用本机合成服务。未明确列出的调试命令会使测试失败。
    fn serve(mut answer: impl FnMut(&Value) -> Value + Send + 'static) -> (Endpoint, thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = Endpoint {
            port: listener.local_addr().unwrap().port(),
            path: "/devtools/browser/01234567-89ab-cdef-0123-456789abcdef".into(),
        };
        let task = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            while let Message::Text(raw) = ws.read().unwrap() {
                let req: Value = serde_json::from_str(&raw).unwrap();
                let mut reply = json!({"id": req["id"], "result": answer(&req)});
                if let Some(session) = req.get("sessionId") { reply["sessionId"] = session.clone(); }
                ws.send(Message::text(reply.to_string())).unwrap();
            }
        });
        (endpoint, task)
    }

    #[test]
    fn doubao_asks_for_an_email_or_single_page_when_tabs_are_ambiguous() {
        let (endpoint, server) = serve(|req| {
            assert_eq!(req["method"], "Target.getTargets");
            json!({"targetInfos": [
                {"type":"page","targetId":"a","url":"https://gemini.google.com/usage"},
                {"type":"page","targetId":"b","url":"https://gemini.google.com/u/1/usage"}
            ]})
        });
        let error = DoubaoConnection::connect_at(&endpoint, None, &AtomicBool::new(false)).err().unwrap();
        assert!(error.contains("Google 邮箱"));
        server.join().unwrap();
    }

    #[test]
    fn doubao_reads_only_quota_and_reuses_the_approved_tab_until_identity_changes() {
        // 后续使用隔离的配置目录，先取得环境锁再启动本地服务，避免等待其他用例时连接超时。
        let _environment = crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut queries = 0;
        let mut detached = false;
        let (endpoint, server) = serve(move |req| match req["method"].as_str().unwrap() {
            "Target.getTargets" if queries == 0 => json!({"targetInfos": [
                {"type":"page","targetId":"other","url":"https://example.invalid/usage"},
                {"type":"page","targetId":"empty","url":"https://gemini.google.com/usage"},
                {"type":"page","targetId":"quota","url":"https://gemini.google.com/u/1/usage"}
            ]}),
            "Target.attachToTarget" if queries == 0 => {
                let target = req["params"]["targetId"].as_str().unwrap();
                assert!(target == "empty" || (target == "quota" && detached));
                json!({"sessionId":target})
            }
            "Target.detachFromTarget" if queries == 0 => {
                assert_eq!(req["params"]["sessionId"], "empty");
                detached = true;
                json!({})
            }
            "Runtime.evaluate" if req["sessionId"] == "empty" => {
                assert_eq!(req["params"]["expression"], super::super::EMAIL_SCRIPT);
                json!({"result":{"value":"other@example.com"}})
            }
            "Runtime.evaluate" => {
                assert_eq!(req["sessionId"], "quota");
                if req["params"]["expression"] == super::super::EMAIL_SCRIPT {
                    let account = if queries < 4 { "fixture@example.com" } else { "changed@example.com" };
                    json!({"result":{"value":account}})
                } else {
                    let expected = format!("({QUERY})({})", json!(request_url("fixture@example.com")));
                    assert_eq!(req["params"]["expression"].as_str(), Some(expected.as_str()));
                    assert_eq!(req["params"]["awaitPromise"], true);
                    queries += 1;
                    if queries == 3 {
                        return json!({"result":{"value":{"error":"limit","retryAfter":85}}});
                    }
                    json!({"result":{"value":{"subscriptionTier":"TIER1","pools":[
                        {"poolType":"GXU_WEEKLY","percentageUsed":0.25,"gxu":{"windowRefreshesAt":1893486251}}
                    ]}}})
                }
            }
            _ => panic!("只允许查询目标标签页的账号和额度"),
        });
        let (conn, account, snapshot) =
            DoubaoConnection::connect_at(&endpoint, Some("fixture@example.com"), &AtomicBool::new(false)).unwrap();
        assert_eq!(account, "fixture@example.com");
        assert_eq!(snapshot.plan, "AI Pro");
        assert_eq!(snapshot.windows[0].remaining, 75.0);
        // 经实际账号记录和后台 fetch 验证刷新、限流、账号切换以及移除后的连接释放。
        super::super::super::tests::assert_doubao_handle_lifecycle(conn, account, snapshot);
        server.join().unwrap();
    }
}
