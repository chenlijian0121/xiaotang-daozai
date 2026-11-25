//! Gemini 网页订阅额度，接口与字段映射参考 CodexBar 的 gemini-web 插件。
//! Chrome 与手动模式通过 DPAPI 保存会话。豆包只保存账号标记，刷新复用用户授权的浏览器连接。
use super::{Account,Snapshot,Window};
use serde_json::{json,Value};
use std::{fs,io::Read,path::PathBuf,sync::{mpsc,Arc,atomic::{AtomicBool,Ordering}}};

#[cfg(windows)]
#[path="gemini_browser.rs"]
mod browser;

const ENDPOINT:&str="https://gemini.google.com/_/BardChatUi/graphql/schemas/GEMINI_WEB_GRAPHQL/executeQuery";
const SIGNATURE:&str="2/dWbCvkVB0X16r9NPnxifeOAAqdCqMf0xeWvau9yPy7g=";
const LOGIN_ERROR:&str="Gemini 登录已失效，请重新连接浏览器账号";
const PARSE_ERROR:&str="Gemini 额度接口已变化，暂时无法读取";

pub fn parse(value:&Value)->Result<Snapshot,String>{
    let data=value.as_array().ok_or(PARSE_ERROR)?.iter().filter_map(|v|v.get("data")).find(|d|d.get("viewer").is_some()).ok_or(PARSE_ERROR)?;
    if data["viewer"].is_null(){return Err(LOGIN_ERROR.into());}
    let usage=data.pointer("/viewer/rpcs/getUsageInfo").ok_or(PARSE_ERROR)?;
    let pools=usage["pools"].as_array().ok_or(PARSE_ERROR)?;
    let windows=[("GXU_FIVE_HOURLY",18000,"5 小时"),("GXU_WEEKLY",604800,"本周")].into_iter().filter_map(|(kind,seconds,label)|{
        let pool=pools.iter().find(|p|p["poolType"]==kind)?;
        let used=pool["percentageUsed"].as_f64().filter(|n|n.is_finite() && *n>=0.0)?;
        Some(Window{label:label.into(),seconds:Some(seconds),remaining:(100.0-used*100.0).clamp(0.0,100.0) as f32,reset:super::reset(&pool["gxu"]["windowRefreshesAt"]),amount:None})
    }).collect::<Vec<_>>();
    if windows.is_empty(){return Err("此 Google 账号暂未提供 Gemini 订阅额度".into());}
    Ok(Snapshot{kiro:None,relay:None,credits:None,wallet:None,plan:if usage["subscriptionTier"]=="TIER1"{"AI Pro"}else{""}.into(),windows,extra:vec![],at:chrono::Utc::now().timestamp()})
}
fn decode(raw:&str)->Result<Snapshot,String>{
    let raw=raw.trim_start();let raw=raw.strip_prefix(")]}'").unwrap_or(raw).trim_start();
    if raw.starts_with('<'){return Err(LOGIN_ERROR.into());}
    parse(&serde_json::from_str::<Value>(raw).map_err(|_|PARSE_ERROR)?)
}
fn email(raw:&str)->Result<String,String>{
    let value=raw.trim().to_ascii_lowercase();
    if value.len()>254 || !value.is_ascii() || value.chars().any(char::is_whitespace) || value.split('@').count()!=2 || !value.split('@').all(|s|!s.is_empty()) {return Err("请填写这个 Google 账号的完整邮箱".into());}
    Ok(value)
}
fn slot(email:&str)->String{ring::digest::digest(&ring::digest::SHA256,email.as_bytes()).as_ref().iter().map(|b|format!("{b:02x}")).collect()}
fn cookie(raw:&str)->Result<String,String>{
    let raw=raw.trim();let raw=raw.strip_prefix("Cookie:").or_else(||raw.strip_prefix("cookie:")).unwrap_or(raw).trim();
    if raw.is_empty() || raw.len()>32768 || !raw.is_ascii() || raw.bytes().any(|b|b<32 || b==127){return Err("请粘贴有效的 Cookie 请求头".into());}
    if !raw.split(';').any(|part|part.trim().split_once('=').is_some_and(|(name,value)|matches!(name,"SID"|"__Secure-1PSID"|"__Secure-3PSID") && !value.is_empty())){return Err("Cookie 中没有 Google 登录会话，请重新复制".into());}
    Ok(raw.into())
}
struct Session {email:String,cookie:String}
fn request_url(email:&str)->String{
    let mut url=url::Url::parse(ENDPOINT).unwrap();
    let variables=json!({"g1Request":{"entrypoints":["GEMINI_USAGE_SETTINGS_UPGRADE_BANNER"]},"workspaceRequest":{"recommendationsForSurfaceRequests":[]}}).to_string();
    url.query_pairs_mut().append_pair("query_signature",SIGNATURE).append_pair("variables",&variables).append_pair("authuser",email);url.into()
}
fn redirect_target(current:&str,location:&str)->Option<String>{
    let target=url::Url::parse(current).ok()?.join(location).ok()?;
    // Google 会把邮箱形式的 authuser 规范化为账号编号；只跟随同站的用量查询。
    (target.scheme()=="https" && target.host_str()==Some("gemini.google.com") && target.port_or_known_default()==Some(443) && target.username().is_empty() && target.password().is_none() && target.path().ends_with("/_/BardChatUi/graphql/schemas/GEMINI_WEB_GRAPHQL/executeQuery")).then(||target.into())
}
fn request(agent:&ureq::Agent,url:&str,session:&Session)->Result<Snapshot,(String,u64)>{
    let mut current=url.to_owned();
    for _ in 0..3{
    let response=agent.get(&current).set("Cookie",&session.cookie).set("Accept","application/json").set("Referer","https://gemini.google.com/usage").set("User-Agent","Mozilla/5.0 Daozai").call().map_err(|e|match e{
        ureq::Error::Status(401|403,_)=>(LOGIN_ERROR.into(),0),
        ureq::Error::Status(429,r)=>("Gemini 请求过于频繁，稍后自动重试".into(),r.header("Retry-After").and_then(|s|s.parse().ok()).unwrap_or(180).clamp(30,3600)),
        ureq::Error::Status(n,_)=>(format!("Gemini 额度服务暂不可用（{n}）"),60),
        _=>("无法连接 Gemini，请检查网络或代理".into(),30)
    })?;
    if (300..400).contains(&response.status()){
        current=response.header("Location").and_then(|to|redirect_target(&current,to)).ok_or_else(||(LOGIN_ERROR.into(),0))?;continue;
    }
    let mut body=String::new();response.into_reader().take(1024*1024).read_to_string(&mut body).map_err(|_|("读取 Gemini 额度失败".into(),30))?;
    return decode(&body).map_err(|e|(e,60));
    }
    Err((LOGIN_ERROR.into(),0))
}
fn session_path(slot:&str)->Result<PathBuf,String>{
    if slot.len()!=64 || !slot.bytes().all(|b|b.is_ascii_hexdigit()){return Err(LOGIN_ERROR.into());}
    Ok(crate::model::config_dir().join("credentials").join(format!("gemini-{slot}.bin")))
}
#[cfg(windows)]
pub(super) fn protect(raw:&[u8],encrypt:bool)->Result<Vec<u8>,String>{
    use windows::Win32::{Foundation::{HLOCAL,LocalFree},Security::Cryptography::{CryptProtectData,CryptUnprotectData,CRYPT_INTEGER_BLOB,CRYPTPROTECT_UI_FORBIDDEN}};
    let input=CRYPT_INTEGER_BLOB{cbData:raw.len().try_into().map_err(|_|LOGIN_ERROR)?,pbData:raw.as_ptr().cast_mut()};let mut output=CRYPT_INTEGER_BLOB::default();
    unsafe{
        if encrypt{CryptProtectData(&input,None,None,None,None,CRYPTPROTECT_UI_FORBIDDEN,&mut output)}else{CryptUnprotectData(&input,None,None,None,None,CRYPTPROTECT_UI_FORBIDDEN,&mut output)}.map_err(|_|"无法访问 Windows 安全保存的登录会话")?;
        let bytes=std::slice::from_raw_parts(output.pbData,output.cbData as usize).to_vec();let _=LocalFree(HLOCAL(output.pbData.cast()));Ok(bytes)
    }
}
#[cfg(not(windows))]
pub(super) fn protect(_raw:&[u8],_encrypt:bool)->Result<Vec<u8>,String>{Err("当前系统暂不支持安全保存登录会话".into())}
/// 磁盘账号记录。旧版 Chrome/手动记录继续使用原有字段；豆包记录只标识账号及连接方式。
enum SavedLogin {
    Cookie(Session),
    Doubao(String),
}

impl SavedLogin {
    /// 返回已核验并规范化的邮箱，用于重连校验和账号槽定位。
    fn address(&self) -> &str {
        match self {
            Self::Cookie(session) => &session.email,
            Self::Doubao(address) => address,
        }
    }

    /// 生成 DPAPI 的明文输入。豆包的浏览器连接不能序列化，账号记录也不含登录凭据。
    fn record(&self) -> Value {
        match self {
            Self::Cookie(session) => json!({"email": session.email, "cookie": session.cookie}),
            Self::Doubao(address) => json!({"email": address, "mode": "doubao"}),
        }
    }

    /// 根据旧版字段或显式模式恢复记录，同时检查记录中的邮箱与配置的槽位一致。
    fn from_record(value: &Value, expected: &str) -> Result<Self, String> {
        let address = email(value["email"].as_str().ok_or(LOGIN_ERROR)?)?;
        if slot(&address) != expected {
            return Err("Gemini 账号不一致，请重新连接".into());
        }
        match (value.get("mode"), value.get("cookie")) {
            (None, Some(header)) => Ok(Self::Cookie(Session {
                email: address,
                cookie: cookie(header.as_str().ok_or(LOGIN_ERROR)?)?,
            })),
            (Some(mode), None) if mode.as_str() == Some("doubao") => Ok(Self::Doubao(address)),
            _ => Err(LOGIN_ERROR.into()),
        }
    }
}

/// 将已验证的记录原子写入该邮箱的 DPAPI 文件；写入失败不替换原记录。
fn save(login:&SavedLogin)->Result<(),String>{
    let path=session_path(&slot(login.address()))?;
    let raw=login.record().to_string();let encrypted=protect(raw.as_bytes(),true)?;
    fs::create_dir_all(path.parent().unwrap()).map_err(|_|"无法保存 Gemini 登录")?;
    crate::model::atomic_write(&path,&encrypted).map_err(|_|"无法保存 Gemini 登录".to_owned())
}
/// 按账号槽加载记录，解密及身份校验均成功后才返回刷新方式。
fn load(account:&Account)->Result<SavedLogin,String>{
    let expected=account.slot.as_deref().ok_or(LOGIN_ERROR)?;let path=session_path(expected)?;
    let raw=fs::read(path).map_err(|_|LOGIN_ERROR)?;let raw=protect(&raw,false)?;
    let value:Value=serde_json::from_slice(&raw).map_err(|_|LOGIN_ERROR)?;
    SavedLogin::from_record(&value, expected)
}
const RECONNECT_DOUBAO: &str = "豆包浏览器连接已断开，请在岛仔重新连接已登录的豆包";

/// 每个账号独立持有已经批准的连接。查表后释放全局锁，避免一个页面等待阻塞其他账号。
#[cfg(windows)]
type LiveDoubao = Arc<std::sync::Mutex<browser::DoubaoConnection>>;
#[cfg(windows)]
static DOUBAO_LINKS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, LiveDoubao>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 用户完成连接、切换方式或移除账号后替换当前句柄。旧连接在全局锁外释放。
#[cfg(windows)]
fn replace_doubao(slot: &str, next: Option<browser::DoubaoConnection>) {
    let previous = {
        let mut links = DOUBAO_LINKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match next {
            Some(connection) => links.insert(slot.to_owned(), Arc::new(std::sync::Mutex::new(connection))),
            None => links.remove(slot),
        }
    };
    drop(previous);
}

/// 刷新仅访问本次进程中用户已批准的句柄。缺少句柄时交给界面提示重连。
#[cfg(windows)]
fn refresh_doubao(address: &str) -> Result<Snapshot, (String, u64)> {
    // 缺少手动授权时无需定时退避；浏览器返回的服务端限流时间则交给共用调度器。
    let disconnected = || (RECONNECT_DOUBAO.to_owned(), 0);
    let connection = DOUBAO_LINKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&slot(address)).cloned().ok_or_else(disconnected)?;
    let mut connection = connection.lock().map_err(|_| disconnected())?;
    connection.refresh(address)
}

/// 移除账号时释放当前授权句柄，并清理该账号的本地记录。
pub fn forget(account:&Account){
    if let Some(slot) = account.slot.as_deref() {
        #[cfg(windows)]
        replace_doubao(slot, None);
        if let Ok(path) = session_path(slot) {let _=fs::remove_file(path);}
    }
}
/// 返回已校验的账号邮箱，供重连表单预填。
pub fn address(account:&Account)->Option<String>{load(account).ok().map(|login|login.address().to_owned())}
/// 通过账号保存的方式刷新额度；豆包只使用现有授权，错误交给界面标记上次记录。
pub fn fetch(agent:&ureq::Agent,account:&Account)->Result<Snapshot,(String,u64)>{
    match load(account).map_err(|e|(e,0))? {
        SavedLogin::Cookie(session) => request(agent, &request_url(&session.email), &session),
        SavedLogin::Doubao(address) => {
            #[cfg(windows)]
            { refresh_doubao(&address) }
            #[cfg(not(windows))]
            { let _ = address; Err((RECONNECT_DOUBAO.into(), 0)) }
        }
    }
}

/// 待界面确认的连接结果；关闭对话框或取消时，未提交的豆包句柄随结果释放。
pub struct Connected {
    pub slot: String,
    pub label: String,
    pub snapshot: Snapshot,
    login: SavedLogin,
    #[cfg(windows)]
    connection: Option<browser::DoubaoConnection>,
}

impl Connected {
    /// 根据用户备注和已核验邮箱构造显示信息，连接提交前不保存任何记录。
    fn new(alias: String, login: SavedLogin, snapshot: Snapshot) -> Self {
        let owner = login.address();
        let label = if alias.trim().is_empty() { format!("Gemini · {owner}") } else { alias.trim().into() };
        Self { slot: slot(owner), label, snapshot, login, #[cfg(windows)] connection: None }
    }

    /// 保存成功后再交接进程内的授权连接，保存失败不会覆盖当前可用的句柄。
    pub fn finish(mut self) -> Result<Self, String> {
        save(&self.login)?;
        #[cfg(windows)]
        match &self.login {
            SavedLogin::Cookie(_) => replace_doubao(&self.slot, None),
            SavedLogin::Doubao(_) => {
                if let Some(connection) = self.connection.take() {
                    replace_doubao(&self.slot, Some(connection));
                }
            }
        }
        Ok(self)
    }
}

/// 指定旧账号重连时，拒绝用另一个浏览器账号覆盖原配置。
fn verify_owner(address: &str, expected: Option<&str>) -> Result<(), String> {
    if expected.is_some_and(|s| s != slot(address)) {
        return Err("当前浏览器是另一个 Google 账号，请切回原账号后重新连接".into());
    }
    Ok(())
}
pub struct Connect {rx:mpsc::Receiver<Result<Connected,String>>,cancel:Arc<AtomicBool>}
impl Connect {
    /// 启动既有 Chrome 或手动连接；expected 为重连时需要匹配的账号槽。
    pub fn start(alias:String,address:String,header:Option<String>,expected:Option<String>)->Self{
        Self::spawn(move |stopped| {
            let session=if let Some(header)=header{Session{email:email(&address)?,cookie:cookie(&header)?}}else{
                #[cfg(windows)]{browser::login(&address,stopped)?}
                #[cfg(not(windows))]{return Err("Chrome 原生连接暂仅支持 Windows，可使用手动 Cookie 连接".into());}
            };
            if stopped.load(Ordering::Relaxed){return Err("已取消 Gemini 连接".into());}
            verify_owner(&session.email, expected.as_deref())?;
            let snapshot=request(&super::http_agent(),&request_url(&session.email),&session).map_err(|e|e.0)?;
            Ok(Connected::new(alias, SavedLogin::Cookie(session), snapshot))
        })
    }

    /// 从用户选择的豆包用量页读取额度。address 可留空，expected 限定旧账号重连的目标。
    pub fn start_doubao(alias: String, address: String, expected: Option<String>) -> Self {
        Self::spawn(move |stopped| {
            #[cfg(windows)]
            {
                let (connection, owner, snapshot) = browser::DoubaoConnection::connect(&address, stopped)?;
                verify_owner(&owner, expected.as_deref())?;
                let mut connected = Connected::new(alias, SavedLogin::Doubao(owner), snapshot);
                connected.connection = Some(connection);
                Ok(connected)
            }
            #[cfg(not(windows))]
            {
                let _ = (alias, address, expected, stopped);
                Err("豆包浏览器原生连接暂仅支持 Windows".into())
            }
        })
    }

    /// 两种连接入口共用可取消的后台任务；通过校验后仍需等待界面调用 finish 才会生效。
    fn spawn(task: impl FnOnce(&AtomicBool) -> Result<Connected, String> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stopped = cancel.clone();
        std::thread::spawn(move || {
            let result = task(&stopped).and_then(|connected| {
                if stopped.load(Ordering::Relaxed) { Err("已取消 Gemini 连接".into()) } else { Ok(connected) }
            });
            let _ = tx.send(result);
        });
        Self { rx, cancel }
    }
    pub fn poll(&self)->Option<Result<Connected,String>>{self.rx.try_recv().ok()}
    #[cfg(test)]
    pub(crate) fn pending()->(Self,mpsc::Sender<Result<Connected,String>>){let(tx,rx)=mpsc::channel();(Self{rx,cancel:Arc::new(AtomicBool::new(false))},tx)}
}
impl Drop for Connect {fn drop(&mut self){self.cancel.store(true,Ordering::Relaxed);}}

#[cfg(test)]
pub(crate) fn simulated_connected()->Connected{
    let session=Session{email:"synthetic@example.com".into(),cookie:"SID=synthetic-only".into()};
    Connected::new("Gemini 测试".into(), SavedLogin::Cookie(session), Snapshot::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 新记录不含登录凭据；旧版记录和账号校验保持兼容，歧义记录不能触发任何一种刷新方式。
    #[test]
    fn gemini_doubao_record_contains_only_browser_and_owner() {
        let address = "do-not-connect@example.com";
        let owner = slot(address);
        let record = SavedLogin::Doubao(address.into()).record();
        assert_eq!(record, json!({"email":address,"mode":"doubao"}));
        assert!(matches!(SavedLogin::from_record(&record, &owner), Ok(SavedLogin::Doubao(value)) if value == address));
        assert!(SavedLogin::from_record(&record, &slot("another@example.com")).is_err());
        let legacy = json!({"email":address,"cookie":"SID=synthetic"});
        assert!(matches!(SavedLogin::from_record(&legacy, &owner), Ok(SavedLogin::Cookie(value)) if value.email == address));
        for invalid in [json!({"email":address,"mode":"other"}), json!({"email":address,"mode":"doubao","cookie":"SID=fixture"})] {
            assert!(SavedLogin::from_record(&invalid, &owner).is_err());
        }
    }

    /// 进程中没有已授权句柄时，后台刷新立即提示从设置重新连接。
    #[cfg(windows)]
    #[test]
    fn gemini_doubao_background_refresh_requires_existing_approval() {
        let error = refresh_doubao("never-connected@example.com").unwrap_err();
        assert_eq!(error, (RECONNECT_DOUBAO.to_owned(), 0));
    }

    /// 使用调用方持有环境锁的合成连接，验证从提交账号到刷新与移除的完整生命周期。
    #[cfg(windows)]
    pub(super) fn assert_doubao_handle_lifecycle(
        connection: browser::DoubaoConnection,
        address: String,
        snapshot: Snapshot,
    ) {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = temp.join(format!("daozai-gemini-handoff-{}-{stamp}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let previous = std::env::var_os("DAOZAI_DATA_DIR");
        std::env::set_var("DAOZAI_DATA_DIR", &dir);
        let owner = slot(&address);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut connected = Connected::new("合成豆包账号".into(), SavedLogin::Doubao(address.clone()), snapshot);
            connected.connection = Some(connection);
            assert!(refresh_doubao(&address).is_err(), "提交前后台不能使用待确认连接");
            connected.finish().unwrap();
            let mut config = super::super::Config::default();
            let id = config.connect_gemini(owner.clone(), "合成豆包账号".into());
            let account = config.accounts.iter().find(|account| account.id == id).unwrap();
            assert_eq!(load(account).unwrap().record(), json!({"email":address,"mode":"doubao"}));
            let agent = super::super::http_agent();
            let refreshed = fetch(&agent, account).unwrap();
            assert_eq!(refreshed.windows[0].remaining, 75.0);
            assert_eq!(refreshed.windows[0].reset, Some(1893486251));
            assert_eq!(fetch(&agent, account).unwrap_err().1, 85);
            let (message, wait) = fetch(&agent, account).unwrap_err();
            assert!(message.contains("切换"));
            assert_eq!(wait, 0);
            forget(account);
            assert!(!session_path(&owner).unwrap().exists());
            assert_eq!(refresh_doubao(&address).unwrap_err(), (RECONNECT_DOUBAO.to_owned(), 0));
        }));
        // 即使断言失败，也要释放授权句柄并恢复进程环境，避免干扰后续用例。
        replace_doubao(&owner, None);
        match previous {
            Some(value) => std::env::set_var("DAOZAI_DATA_DIR", value),
            None => std::env::remove_var("DAOZAI_DATA_DIR"),
        }
        assert_eq!(dir.canonicalize().unwrap().parent(), Some(temp.as_path()));
        fs::remove_dir_all(&dir).unwrap();
        if let Err(error) = result { std::panic::resume_unwind(error); }
    }

    fn response(five:Value,week:Value)->Value{json!([{"data":{"viewer":{"rpcs":{"getUsageInfo":{"subscriptionTier":"TIER1","pools":[{"poolType":"GXU_FIVE_HOURLY","percentageUsed":five,"gxu":{"windowRefreshesAt":"2030-01-01T08:24:11Z"}},{"poolType":"GXU_WEEKLY","percentageUsed":week,"gxu":{"windowRefreshesAt":"2030-01-04T03:24:11Z"}}]}}}},"errors":[{"message":"unrelated upsell"}]}])}
    #[test] fn gemini_fractional_usage_and_real_resets(){let s=parse(&response(json!(0.24),json!(0.01786011))).unwrap();assert_eq!(s.plan,"AI Pro");assert_eq!(s.rings().len(),2);assert_eq!(s.windows[0].remaining,76.0);assert!((s.windows[1].remaining-98.21399).abs()<0.001);assert_eq!(s.windows[0].reset,Some(1893486251));assert_eq!(s.windows[0].seconds,Some(18000));assert_eq!(s.windows[1].seconds,Some(604800));}
    #[test] fn gemini_zero_exhaustion_and_missing_windows(){let s=parse(&response(json!(0),json!(1))).unwrap();assert_eq!(s.windows[0].remaining,100.0);assert_eq!(s.windows[1].remaining,0.0);for bad in [Value::Null,json!(-1),json!("0.25"),json!("NaN")]{let s=parse(&response(bad.clone(),json!(0.5))).unwrap();assert_eq!(s.rings().len(),1);assert_eq!(s.rings()[0].label,"本周");assert!(parse(&response(bad.clone(),bad)).is_err());}assert_eq!(parse(&response(json!(1.2),Value::Null)).unwrap().windows[0].remaining,0.0);}
    #[test] fn gemini_signed_out_or_changed_queries_are_not_full_quota(){for raw in [r#"[{"data":{"viewer":null}}]"#,r#"[{"data":{}}]"#,"{}","[]",r#"[{"data":{"viewer":{"rpcs":{"getUsageInfo":{"pools":[]}}}}}]"#]{assert!(decode(raw).is_err());}let raw=format!(")]}}'\n{}",response(json!(0.3),json!(0.2)));assert_eq!(decode(&raw).unwrap().windows[0].remaining,70.0);assert!(decode("<html>private</html>").unwrap_err().contains("登录"));}
    #[test] fn gemini_unknown_plan_and_reset_stay_unknown(){let mut v=response(json!(0.1),Value::Null);let usage=&mut v[0]["data"]["viewer"]["rpcs"]["getUsageInfo"];usage["subscriptionTier"]=json!("NEW_TIER");usage["pools"][0]["gxu"]["windowRefreshesAt"]=json!("unknown");let s=parse(&v).unwrap();assert!(s.plan.is_empty());assert!(s.windows[0].reset.is_none());}
    #[test] fn gemini_account_selection_and_cookie_validation(){let e=email(" Personal@Example.COM ").unwrap();assert_eq!(e,"personal@example.com");let u=url::Url::parse(&request_url(&e)).unwrap();let pairs=u.query_pairs().collect::<std::collections::HashMap<_,_>>();assert_eq!(pairs["authuser"],e);assert_eq!(pairs["query_signature"],SIGNATURE);assert_eq!(u.host_str(),Some("gemini.google.com"));assert_ne!(slot(&e),slot("work@example.com"));for bad in ["", "test", "a@b\r\nInjected: yes"]{assert!(email(bad).is_err());}assert_eq!(cookie("Cookie: SID=test; SAPISID=other").unwrap(),"SID=test; SAPISID=other");for bad in ["", "SID=test\r\nX: bad", "foo=bar"]{assert!(cookie(bad).is_err());}assert!(session_path("../../other").is_err());}
    #[test] fn gemini_account_redirects_never_forward_cookies_to_another_site(){let url=request_url("personal@example.com");let target=redirect_target(&url,"?authuser=0").unwrap();assert_eq!(url::Url::parse(&target).unwrap().query(),Some("authuser=0"));for to in ["https://accounts.google.com/signin","https://gemini.google.com.evil.invalid/","http://gemini.google.com/","https://gemini.google.com:444/","https://user@gemini.google.com/","/app"]{assert!(redirect_target(&url,to).is_none());}}
    #[cfg(windows)] #[test] fn gemini_windows_storage_is_encrypted_and_roundtrips(){let raw=b"{\"email\":\"synthetic@example.com\",\"cookie\":\"SID=synthetic-secret\"}";let sealed=protect(raw,true).unwrap();assert!(!sealed.windows(16).any(|w|w==b"synthetic-secret"));assert_eq!(protect(&sealed,false).unwrap(),raw);let mut corrupt=sealed;corrupt[24]^=1;assert!(protect(&corrupt,false).is_err());}
    #[test] fn gemini_read_only_request_does_not_echo_private_errors(){
        use std::net::TcpListener;let server=TcpListener::bind("127.0.0.1:0").unwrap();let url=format!("http://{}/quota",server.local_addr().unwrap());
        let task=std::thread::spawn(move||{let(mut stream,_)=server.accept().unwrap();let mut bytes=[0u8;4096];let n=stream.read(&mut bytes).unwrap();let raw=String::from_utf8_lossy(&bytes[..n]);assert!(raw.starts_with("GET /quota HTTP/1.1"));assert!(raw.to_ascii_lowercase().contains("cookie: sid=synthetic"));use std::io::Write;stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 14\r\nConnection: close\r\n\r\nprivate-secret").unwrap();});
        let session=Session{email:"synthetic@example.com".into(),cookie:"SID=synthetic".into()};let error=request(&ureq::AgentBuilder::new().redirects(0).build(),&url,&session).unwrap_err();assert!(error.0.contains("重新连接"));assert!(!error.0.contains("private-secret"));task.join().unwrap();
    }
    #[test]
    #[ignore="按需只读验证已有 Gemini 加密会话，不更新配置"]
    fn gemini_saved_session_live_read_only(){
        let _g=crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let config=super::super::Config::load();let a=config.accounts.iter().find(|a|a.enabled&&a.provider==super::super::Provider::Gemini).expect("需要已有 Gemini 账号");
        let snapshot=fetch(&super::super::http_agent(),a).unwrap();assert!(!snapshot.windows.is_empty());
        println!("已有 Gemini 加密会话读取成功，返回 {} 个额度周期",snapshot.windows.len());
    }
    #[cfg(windows)] #[test]
    #[ignore="使用指定隔离目录验证捕获会话，写入隔离账号配置"]
    fn gemini_captured_session_independent_refresh(){
        let _g=crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        assert!(std::env::var_os("DAOZAI_DATA_DIR").is_some(),"真实验证必须指定隔离测试目录");
        let slot=fs::read_to_string(crate::model::config_dir().join("verified-slot.txt")).unwrap();
        let mut config=super::super::Config::default();let id=config.connect_gemini(slot,"Gemini 真实验证".into());
        let account=config.accounts.iter().find(|a|a.id==id).unwrap();let snapshot=fetch(&super::super::http_agent(),account).unwrap();
        assert_eq!(snapshot.windows.len(),2);config.save_checked().unwrap();
        println!("浏览器已断开，原生查询成功：{:?}",snapshot.windows.iter().map(|w|(&w.label,w.remaining)).collect::<Vec<_>>());
    }
    #[cfg(windows)] #[test]
    #[ignore="需要本人允许 Chrome 原生连接，仅更新指定隔离目录中的会话"]
    fn gemini_native_chrome_live_connection(){
        let _g=crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        assert!(std::env::var_os("DAOZAI_DATA_DIR").is_some(),"真实验证必须指定隔离测试目录");
        let config=super::super::Config::load();let account=config.accounts.iter().find(|a|a.provider==super::super::Provider::Gemini).unwrap();
        let connect=Connect::start(account.alias.clone(),address(account).unwrap(),None,account.slot.clone());
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(315);
        let result=loop{if let Some(result)=connect.poll(){break result;}assert!(std::time::Instant::now()<deadline,"Chrome 授权等待超时");std::thread::sleep(std::time::Duration::from_millis(50));};
        let connected=result.unwrap().finish().unwrap();assert_eq!(connected.snapshot.windows.len(),2);
        assert_eq!(Some(connected.slot),account.slot);assert!(fetch(&super::super::http_agent(),account).is_ok());
        println!("原生 Rust 授权连接、身份校验、加密保存及独立刷新全部通过");
    }
}
