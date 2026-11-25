use serde_json::{json, Value};

use super::{parse, Provider, Snapshot};

/// 将 App Server 的主、副窗口转成现有 Codex 解析器使用的字段。协议周期以分钟计，
/// 缺失或溢出的周期保留为空，缺失的窗口也不生成虚构的使用量。
fn rate_windows(bucket: &Value) -> Value {
    let window = |value: &Value| {
        if value.is_null() {
            return Value::Null;
        }
        let seconds = value["windowDurationMins"].as_i64().and_then(|n| n.checked_mul(60));
        json!({
            "used_percent": value["usedPercent"],
            "limit_window_seconds": seconds,
            "reset_at": value["resetsAt"],
        })
    };
    json!({
        "primary_window": window(&bucket["primary"]),
        "secondary_window": window(&bucket["secondary"]),
    })
}

/// 读取 account/rateLimits/read 的 result 并返回岛仔的额度快照。
/// 新协议按桶标识区分主额度与附加额度；旧协议只提供单桶，主额度缺失时返回错误。
pub(super) fn parse_codex_app_limits(result: &Value) -> Result<Snapshot, String> {
    let buckets = result["rateLimitsByLimitId"].as_object();
    let main = buckets.and_then(|all| all.get("codex")).filter(|v| v.is_object())
        .or_else(|| {
            let legacy = &result["rateLimits"];
            let id = legacy["limitId"].as_str();
            (legacy.is_object() && id.is_none_or(|id| id == "codex")).then_some(legacy)
        })
        .ok_or("Codex 未返回当前账号的主额度")?;

    // 兼容视图与新协议中的 codex 桶表示同一组额度，只在主窗口中展示一次。
    let extra: Vec<_> = buckets.into_iter().flatten()
        .filter(|(id, _)| id.as_str() != "codex")
        .map(|(id, bucket)| json!({
            "limit_name": bucket["limitName"].as_str().filter(|name| !name.is_empty()).unwrap_or(id),
            "rate_limit": rate_windows(bucket),
        }))
        .collect();
    parse(Provider::Codex, &json!({
        "plan_type": main["planType"],
        "rate_limit": rate_windows(main),
        "additional_rate_limits": extra,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_app_limits_prefer_named_main_and_keep_other_buckets_separate() {
        let data = json!({
            "rateLimits": {"limitId": "codex", "primary": {"usedPercent": 99}},
            "rateLimitsByLimitId": {
                "codex": {"planType": "plus", "primary": null, "secondary": {
                    "usedPercent": 30, "windowDurationMins": 10080, "resetsAt": 1730947200
                }},
                "review": {"primary": {"usedPercent": 42, "windowDurationMins": 60}}
            }
        });
        let snapshot = parse_codex_app_limits(&data).unwrap();
        assert_eq!(snapshot.plan, "plus");
        assert_eq!(snapshot.windows.len(), 1);
        let main = &snapshot.windows[0];
        assert_eq!((main.seconds, main.remaining, main.reset), (Some(604800), 70.0, Some(1730947200)));
        assert_eq!(snapshot.extra.len(), 1);
        assert_eq!(snapshot.extra[0].label, "review · 1 小时");
        assert_eq!(snapshot.extra[0].remaining, 58.0);
    }

    #[test]
    fn codex_app_limits_support_legacy_window_with_optional_metadata() {
        let data = json!({"rateLimits": {"primary": {"usedPercent": 0}, "secondary": null}});
        let snapshot = parse_codex_app_limits(&data).unwrap();
        assert_eq!(snapshot.windows.len(), 1);
        let main = &snapshot.windows[0];
        assert_eq!((main.seconds, main.remaining, main.reset), (None, 100.0, None));
        assert!(snapshot.extra.is_empty());
    }

    #[test]
    fn codex_app_limits_require_valid_main_usage() {
        for data in [
            json!({"rateLimits": {"limitId": "other", "primary": {"usedPercent": 10}}}),
            json!({"rateLimitsByLimitId": {"review": {"primary": {"usedPercent": 10}}}}),
            json!({"rateLimits": {"primary": null, "secondary": null}}),
            json!({"rateLimits": {"primary": {"usedPercent": "NaN"}}}),
        ] {
            assert!(parse_codex_app_limits(&data).is_err());
        }
    }
}
