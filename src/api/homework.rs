//! 课堂小测（`workType=1`）：清单、作答快照、作答状态。
//!
//! ## 这里没有的东西，都是有原因的
//!
//! - **没有「截止时间」字段**。全量响应里 `endTime`/`workEndTime` 恒为 `null`，
//!   前端也就不按时间关门——小测是否可做只看服务端的作答锁（见下）。
//! - **没有「重新提交」**。已提交过的小测，`answer/submit` 一律回 `40002`。
//!   这不是前端限制：改 `submitStatus`、删本地记录、重放 `work/enter` 都试过，
//!   服务端锁死。所以本模块只提供读，不提供写。
//!
//! ## `status` 是批阅状态
//!
//! 清单接口的 `status` 参数**不是**「交没交」：
//!
//! | `status` | 含义 | 实测（24 条） |
//! | --- | --- | --- |
//! | 0 | 待提交 | 0 条 |
//! | 1 | 已交、待批阅 | 3 条（`rateStatus=0`） |
//! | 2 | 已交、已批阅 | 21 条（`rateStatus=1`） |
//!
//! 而且它**必填**——不给就 `{code:100, value:null}`（不是空列表，是错误码）。
//! 要拿全集只能把 1 和 2 各拉一遍再合并，[`fetch_all`] 就是这么做的。
//!
//! ## 分数口径
//!
//! `score` 是百分制小数（6.0 / 5.55），`totalPoints` 是满分（6 / 10），
//! `pointSystem` 是放大倍数（`600` / `1000`）。**`score` 已经是分数本身**，
//! 不要再除以 `pointSystem`——那是给前端画进度条的。

use serde_json::{Value, json};

use crate::endpoints::{HW_ANSWER_LOAD, HW_WORK_ENTER, HW_WORK_LIST, portal_url};
use crate::error::{Error, Result};
use crate::transport::{HostIdentity, Transport, body_message, portal_ok};

/// 清单里的 `status`（批阅状态）取值。
pub const STATUS_PENDING: i64 = 0;
pub const STATUS_UNRATED: i64 = 1;
pub const STATUS_RATED: i64 = 2;

/// 一节课后小测。
#[derive(Clone, Debug, PartialEq)]
pub struct Work {
    /// 作业 ID（19 位数字串）。`answer/load`、`work/enter` 都用它。
    pub id: String,
    pub name: String,
    /// `workType`。本模块拿到的恒为 1（课堂小测）。
    pub work_type: i64,
    pub course_id: String,
    pub course_name: String,
    /// 所在班级。一个作业可能挂多个班，这里保留全部。
    pub classes: Vec<WorkClass>,
    pub teacher_name: String,
    /// 满分。`score` 的对照值。
    pub total_points: f64,
    /// 放大倍数（600 / 1000），供前端画条用，**不要**拿它折算 `score`。
    pub point_system: i64,
    /// 得分。百分制小数；未批阅时为 `None`。
    pub score: Option<f64>,
    /// 提交状态：0 未提交、2 已提交。
    pub submit_status: i64,
    /// 批阅状态：0 待批阅、1 已批阅。
    pub rate_status: i64,
    /// 创建时间（毫秒时间戳）。
    pub create_time: i64,
    /// 开放作答时间（毫秒时间戳）。
    pub start_time: i64,
    /// 提交时间（毫秒时间戳）。未交为 `None`。
    pub submit_time: Option<i64>,
    /// 限时作答（秒）。0 = 不限时。
    pub answer_time_limit: i64,
    /// 服务端是否还允许作答（`canAnswer`）。已交过恒为 0。
    pub can_answer: i64,
    /// 是否允许重做（`redoFlag`）。已交过恒为 0。
    pub redo_flag: i64,
    /// 是否已过截止（`overdueFlag`）。小测恒为 0。
    pub overdue_flag: i64,
}

/// 作业挂靠的班级。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkClass {
    pub class_id: String,
    pub class_name: String,
}

impl Work {
    /// 是否已提交。
    pub fn is_submitted(&self) -> bool {
        self.submit_status == 2
    }

    /// 是否已批阅。
    pub fn is_rated(&self) -> bool {
        self.rate_status == 1
    }

    /// 得分率（0..=1）。未批阅或满分为 0 时是 `None`。
    pub fn ratio(&self) -> Option<f64> {
        if self.total_points <= 0.0 {
            None
        } else {
            self.score.map(|score| score / self.total_points)
        }
    }
}

/// 一道题的作答。`answer` 原文是**再套一层的 JSON 字符串**，这里已经解开。
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// 题型实例 ID。和目录里的 `instanceId` 是同一套编号。
    pub instance_id: String,
    /// 解开的作答结构。解不开时保留原始字符串，塞进 `raw`。
    pub value: Option<Value>,
    pub raw: String,
}

impl Answer {
    /// 从 `answer` 字符串解析。非 JSON 不报错——原样保留，让上层自己决定怎么显示。
    pub fn parse(instance_id: &str, raw: &str) -> Self {
        let value = serde_json::from_str::<Value>(raw).ok();
        Self {
            instance_id: instance_id.to_owned(),
            value,
            raw: raw.to_owned(),
        }
    }

    /// 用户填的文本（`children[].value[]` 里非空的那些）。
    ///
    /// 填空/选择都在这里；纯录音题会是空向量。
    pub fn filled_texts(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(children) = self.value.as_ref().and_then(|v| v.get("children")) {
            if let Some(items) = children.as_array() {
                for child in items {
                    if let Some(values) = child.get("value").and_then(Value::as_array) {
                        for item in values {
                            if let Some(text) = item.as_str()
                                && !text.trim().is_empty()
                            {
                                out.push(text.to_owned());
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// 附件（图片/音频）URL 列表，来自 `children[].fileList[].url`。
    pub fn file_urls(&self) -> Vec<String> {
        self.collect_urls("fileList")
    }

    /// 录音 URL 列表，来自 `children[].record.url`。
    pub fn record_urls(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(children) = self.value.as_ref().and_then(|v| v.get("children")) {
            if let Some(items) = children.as_array() {
                for child in items {
                    if let Some(url) = child
                        .get("record")
                        .and_then(|r| r.get("url"))
                        .and_then(Value::as_str)
                        && !url.trim().is_empty()
                    {
                        out.push(url.to_owned());
                    }
                }
            }
        }
        out
    }

    fn collect_urls(&self, key: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(children) = self.value.as_ref().and_then(|v| v.get("children")) {
            if let Some(items) = children.as_array() {
                for child in items {
                    if let Some(list) = child.get(key).and_then(Value::as_array) {
                        for file in list {
                            if let Some(url) = file.get("url").and_then(Value::as_str)
                                && !url.trim().is_empty()
                            {
                                out.push(url.to_owned());
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

/// 拉一个 `status` 分页的清单。`status` 见模块文档（0/1/2）。
pub fn fetch(
    client: &reqwest::blocking::Client,
    portal_token: &str,
    host: &HostIdentity,
    status: i64,
) -> Result<Vec<Work>> {
    let transport = Transport::portal(client, portal_token).with_host(host.clone());
    let body = transport.post_json(
        &portal_url(HW_WORK_LIST),
        &json!({
            "workName": "",
            "workType": 1,
            "pageNum": 1,
            "pageSize": 200,
            "status": status,
        }),
    )?;
    check_ok(&body, "读取小测清单")?;
    Ok(parse_list(&body))
}

/// 拉全部小测（`status=1` 与 `status=2` 合并去重）。
///
/// 不传 `status=0`：本账号恒为空，且多一次请求就多一次限流机会。
/// 若以后真有未交的，它会在 `status=0` 里——届时再加。
pub fn fetch_all(
    client: &reqwest::blocking::Client,
    portal_token: &str,
    host: &HostIdentity,
) -> Result<Vec<Work>> {
    let mut all: Vec<Work> = fetch(client, portal_token, host, STATUS_UNRATED)?;
    for work in fetch(client, portal_token, host, STATUS_RATED)? {
        if !all.iter().any(|existing| existing.id == work.id) {
            all.push(work);
        }
    }
    // 按提交时间倒序；没交的排在最后。
    all.sort_by(|a, b| {
        let left = a.submit_time.or(Some(a.create_time)).unwrap_or(0);
        let right = b.submit_time.or(Some(b.create_time)).unwrap_or(0);
        right.cmp(&left)
    });
    Ok(all)
}

/// 读一个作业的作答快照。已交/未交都能读。
pub fn fetch_answers(
    client: &reqwest::blocking::Client,
    portal_token: &str,
    host: &HostIdentity,
    work_id: &str,
) -> Result<WorkAnswers> {
    let transport = Transport::portal(client, portal_token).with_host(host.clone());
    let body = transport.post_json(&portal_url(HW_ANSWER_LOAD), &json!({ "workId": work_id }))?;
    check_ok(&body, "读取作答")?;
    Ok(parse_answers(&body))
}

/// 作答快照。
#[derive(Clone, Debug, PartialEq)]
pub struct WorkAnswers {
    /// 累计作答时长（秒）。
    pub duration: i64,
    /// 作业版本号。
    pub version_id: i64,
    /// 题目作答，顺序与页面一致。
    pub answers: Vec<Answer>,
}

impl WorkAnswers {
    /// 是否真的动过手——有任何一段文本、附件或录音。
    pub fn is_blank(&self) -> bool {
        self.answers.iter().all(|answer| {
            answer.filled_texts().is_empty()
                && answer.file_urls().is_empty()
                && answer.record_urls().is_empty()
        })
    }
}

/// 服务端是否还允许作答。
///
/// ## 为什么用 `work/enter` 探
///
/// 清单里的 `canAnswer`/`redoFlag` 已经够用了；但要让「能不能再做」这个判断
/// 落到**服务端当下**，只有发一次 `work/enter`——它是幂等的读操作，已交回
/// `40004`，未交回 `code:1`。`submit` 撞 `40002` 结论相同，但那是写请求。
///
/// 返回 `Ok(true)` = 可作答。
pub fn can_answer(
    client: &reqwest::blocking::Client,
    portal_token: &str,
    host: &HostIdentity,
    work_id: &str,
) -> Result<bool> {
    let transport = Transport::portal(client, portal_token).with_host(host.clone());
    let body = transport.post_json(&portal_url(HW_WORK_ENTER), &json!({ "workId": work_id }))?;

    let code = body.get("code").and_then(Value::as_i64).unwrap_or(0);
    // 40004 = 已提交。这是**正常结论**，不是错误。
    if code == 40004 {
        return Ok(false);
    }
    if !portal_ok(&body) {
        return Err(Error::api(format!(
            "检测作答状态失败（code {code}）：{}",
            body_message(&body, "服务端未给原因")
        )));
    }
    Ok(true)
}

/// 门户响应检查：`code == 1` 才算成功。
fn check_ok(body: &Value, what: &str) -> Result<()> {
    if portal_ok(body) {
        return Ok(());
    }
    let code = body
        .get("code")
        .and_then(Value::as_i64)
        .map(|code| code.to_string())
        .unwrap_or_else(|| "?".to_owned());
    Err(Error::api(format!(
        "{what}失败（code {code}）：{}",
        body_message(body, "服务端未给原因")
    )))
}

fn parse_list(body: &Value) -> Vec<Work> {
    body.get("value")
        .and_then(|value| value.get("dataList"))
        .and_then(Value::as_array)
        .map(|items| items.iter().map(parse_work).collect())
        .unwrap_or_default()
}

fn parse_work(item: &Value) -> Work {
    Work {
        id: text(item, "id"),
        name: text(item, "name"),
        work_type: int(item, "workType"),
        course_id: text(item, "courseId"),
        course_name: text(item, "courseName"),
        classes: item
            .get("classInfoList")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|class| WorkClass {
                        class_id: text(class, "classId"),
                        class_name: text(class, "className"),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        teacher_name: text(item, "teacherName"),
        total_points: number(item, "totalPoints"),
        point_system: int(item, "pointSystem"),
        score: item.get("score").and_then(Value::as_f64),
        submit_status: int(item, "submitStatus"),
        rate_status: int(item, "rateStatus"),
        create_time: int(item, "createTime"),
        start_time: int(item, "startTime"),
        submit_time: item.get("submitTime").and_then(Value::as_i64),
        answer_time_limit: int(item, "answerTimeLimit"),
        can_answer: int(item, "canAnswer"),
        redo_flag: int(item, "redoFlag"),
        overdue_flag: int(item, "overdueFlag"),
    }
}

fn parse_answers(body: &Value) -> WorkAnswers {
    let value = body.get("value").cloned().unwrap_or(Value::Null);
    WorkAnswers {
        duration: int(&value, "duration"),
        version_id: int(&value, "versionId"),
        answers: value
            .get("quesDataList")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| Answer::parse(&text(item, "instanceId"), &text(item, "answer")))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn text(item: &Value, key: &str) -> String {
    match item.get(key) {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

fn int(item: &Value, key: &str) -> i64 {
    item.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn number(item: &Value, key: &str) -> f64 {
    item.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATED: &str = r#"{"code":1,"value":{"total":1,"dataList":[{
        "id":"1018594796116722056","name":"U3 IC What is cop","workType":1,
        "courseId":"25013","courseName":"全球胜任力英语教程(春)",
        "classInfoList":[{"classId":"1824551422230249472","className":"26安全工程"}],
        "teacherName":"彭圆","totalPoints":6,"pointSystem":600,"score":6.0,
        "submitStatus":2,"rateStatus":1,"createTime":1793630335000,
        "startTime":1793667852000,"submitTime":1793668068000,
        "answerTimeLimit":0,"canAnswer":0,"redoFlag":0,"overdueFlag":0
    }]}}"#;

    const UNRATED: &str = r#"{"code":1,"value":{"total":1,"dataList":[{
        "id":"1018593902763494247","name":"U3 warm up speaking","workType":1,
        "courseId":"25013","courseName":"全球胜任力英语教程(春)",
        "classInfoList":[],"teacherName":"彭圆","totalPoints":5,"pointSystem":500,
        "score":null,"submitStatus":2,"rateStatus":0,"createTime":1792460630000,
        "startTime":1792460630000,"submitTime":1792460630000,
        "answerTimeLimit":0,"canAnswer":0,"redoFlag":0,"overdueFlag":0
    }]}}"#;

    #[test]
    fn parses_rated_work() {
        let body: Value = serde_json::from_str(RATED).unwrap();
        let works = parse_list(&body);
        assert_eq!(works.len(), 1);
        let work = &works[0];
        assert_eq!(work.id, "1018594796116722056");
        assert_eq!(work.total_points, 6.0);
        assert_eq!(work.score, Some(6.0));
        assert!(work.is_submitted());
        assert!(work.is_rated());
        assert_eq!(work.ratio(), Some(1.0));
        assert_eq!(work.classes[0].class_name, "26安全工程");
    }

    #[test]
    fn unrated_work_has_no_ratio() {
        let body: Value = serde_json::from_str(UNRATED).unwrap();
        let work = &parse_list(&body)[0];
        assert_eq!(work.score, None);
        assert!(!work.is_rated());
        assert_eq!(work.ratio(), None);
    }

    #[test]
    fn missing_value_is_empty_list() {
        let body: Value = serde_json::from_str(r#"{"code":100,"value":null}"#).unwrap();
        assert!(parse_list(&body).is_empty());
        assert!(check_ok(&body, "读取小测清单").is_err());
    }

    #[test]
    fn splits_answer_text_files_and_records() {
        let raw = r#"{"value":[],"children":[
            {"value":["consumption"],"isDone":true},
            {"value":[""],"fileList":[{"url":"http://x/a.jpg","name":"a.jpg"}],
             "record":{"url":"http://x/b.mp3"}}
        ],"record":{"url":""}}"#;
        let answer = Answer::parse("2016775698586132480", raw);
        assert_eq!(answer.filled_texts(), vec!["consumption".to_owned()]);
        assert_eq!(answer.file_urls(), vec!["http://x/a.jpg".to_owned()]);
        assert_eq!(answer.record_urls(), vec!["http://x/b.mp3".to_owned()]);
    }

    #[test]
    fn blank_answer_is_blank() {
        let raw = r#"{"value":[],"children":[{"value":[""]}],"record":{"url":""}}"#;
        let parsed = WorkAnswers {
            duration: 0,
            version_id: 1,
            answers: vec![Answer::parse("1", raw)],
        };
        assert!(parsed.is_blank());
    }

    #[test]
    fn non_json_answer_is_kept_raw() {
        let answer = Answer::parse("1", "not-json");
        assert!(answer.value.is_none());
        assert_eq!(answer.raw, "not-json");
        assert!(answer.filled_texts().is_empty());
    }
}
