//! MCP(Model Context Protocol) 서버.
//!
//! Claude Desktop은 터미널 명령을 실행할 수 없다. 일반 사용자가 Claude에게
//! "그 파일 어느 하드에 있어?"라고 물으려면 이 통로가 필요하다.
//!
//! stdio 전송은 줄 단위 JSON-RPC 2.0이 전부다. 요청 한 줄을 읽고 응답 한 줄을
//! 쓰는 구조라 비동기 런타임이 필요 없다.
//!
//! stdout은 프로토콜 전용이다. 진단 메시지는 반드시 stderr로 보낸다.

use anyhow::Result;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

use crate::db;

/// 클라이언트가 버전을 알려 주지 않을 때 쓸 프로토콜 버전.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// JSON-RPC 오류 코드: 없는 메서드.
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC 오류 코드: 잘못된 인자.
const INVALID_PARAMS: i64 = -32602;

/// 이 서버가 노출하는 도구 정의.
fn tool_definitions() -> Value {
    json!([
        {
            "name": "search_files",
            "description": "외장하드에 보관된 파일과 폴더를 이름으로 검색하고, 어느 하드에 있는지 알려줍니다. 하드가 연결되어 있지 않아도 검색됩니다. 결과의 drive_label이 사용자가 물리적으로 찾아야 할 하드 이름입니다.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keyword": {
                        "type": "string",
                        "description": "찾을 파일이나 폴더 이름의 일부. 경로에 들어 있어도 찾습니다."
                    },
                    "drive": {
                        "type": "string",
                        "description": "특정 하드 안에서만 검색할 때 그 하드의 라벨."
                    },
                    "dirs_only": {
                        "type": "boolean",
                        "description": "폴더만 검색합니다. 프로젝트 단위로 찾을 때 씁니다. 기본값 false."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "결과 개수 상한. 기본값 50."
                    }
                },
                "required": ["keyword"]
            }
        },
        {
            "name": "list_drives",
            "description": "인덱싱된 외장하드 목록을 보여줍니다. 각 하드의 라벨, 파일 시스템 형식(NTFS·exFAT·FAT32 등), 보관된 항목 수, 용량, 마지막 연결 시각과 함께 지금 컴퓨터에 연결되어 있는지 여부를 알려줍니다. 어떤 하드를 어떤 형식으로 포맷해 두었는지 물어볼 때도 이 도구를 쓰세요.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_status",
            "description": "인덱스 전체 통계를 보여줍니다. 등록된 하드 수, 인덱싱된 파일과 폴더 수, 인덱스 파일 크기입니다.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

/// 도구 호출을 실제로 수행하고 사람이 읽을 결과 문자열을 만든다.
fn call_tool(name: &str, args: &Value) -> Result<String> {
    let conn = db::open()?;

    match name {
        "search_files" => {
            let keyword = args
                .get("keyword")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("keyword 인자가 필요합니다"))?;
            let drive = args.get("drive").and_then(Value::as_str);
            let dirs_only = args.get("dirs_only").and_then(Value::as_bool).unwrap_or(false);
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50).min(500) as usize;

            let hits = db::search(&conn, keyword, drive, dirs_only, limit)?;
            if hits.is_empty() {
                let stats = db::stats(&conn)?;
                if stats.entry_count == 0 {
                    return Ok("인덱싱된 하드가 없습니다. 외장하드를 연결하면 자동으로 인덱싱됩니다.".into());
                }
                return Ok(format!("'{keyword}'에 해당하는 자료를 찾지 못했습니다."));
            }

            let mut labels: Vec<&str> = hits.iter().map(|h| h.drive_label.as_str()).collect();
            labels.sort_unstable();
            labels.dedup();

            Ok(serde_json::to_string_pretty(&json!({
                "found": hits.len(),
                "truncated": hits.len() == limit,
                "drives_to_connect": labels,
                "results": hits,
            }))?)
        }

        "list_drives" => {
            let drives = db::list_drives(&conn)?;
            if drives.is_empty() {
                return Ok("아직 인덱싱된 하드가 없습니다.".into());
            }
            Ok(serde_json::to_string_pretty(&drives)?)
        }

        "get_status" => Ok(serde_json::to_string_pretty(&db::stats(&conn)?)?),

        other => Err(anyhow::anyhow!("알 수 없는 도구입니다: {other}")),
    }
}

/// 성공 응답을 만든다.
fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// 오류 응답을 만든다.
fn err_response(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// 요청 한 건을 처리한다.
///
/// 응답이 필요 없는 알림(notification)이면 `None`을 돌려준다.
fn handle(request: &Value) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str)?;
    // 알림에는 id가 없다. 응답을 보내면 안 되므로 여기서 빠져나간다.
    let id = request.get("id").cloned()?;

    match method {
        "initialize" => {
            // 클라이언트가 말한 버전을 그대로 받아 준다. 서로 아는 버전으로 맞추는 가장 안전한 방법이다.
            let version = request
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);

            Some(ok_response(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "drive-archive",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            ))
        }

        "ping" => Some(ok_response(id, json!({}))),

        "tools/list" => Some(ok_response(id, json!({ "tools": tool_definitions() }))),

        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(err_response(id, INVALID_PARAMS, "도구 이름이 없습니다".into()));
            };
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

            // 도구 실행 실패는 프로토콜 오류가 아니다. isError로 알려 주면
            // 모델이 무엇이 잘못됐는지 읽고 다시 시도할 수 있다.
            let (text, is_error) = match call_tool(name, &args) {
                Ok(t) => (t, false),
                Err(e) => (format!("오류: {e:#}"), true),
            };

            Some(ok_response(
                id,
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_error,
                }),
            ))
        }

        other => Some(err_response(
            id,
            METHOD_NOT_FOUND,
            format!("지원하지 않는 메서드입니다: {other}"),
        )),
    }
}

/// stdin에서 요청을 읽어 stdout으로 응답하는 루프.
///
/// 클라이언트가 stdin을 닫으면 정상 종료한다.
pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Value>(line) {
            Ok(request) => handle(&request),
            Err(e) => {
                // 파싱조차 안 되는 줄은 어떤 요청에 대한 것인지 알 수 없다.
                eprintln!("drive-archive mcp: 요청을 해석할 수 없습니다: {e}");
                continue;
            }
        };

        if let Some(response) = response {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, id: i64) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method })
    }

    #[test]
    fn initialize는_클라이언트_버전을_그대로_돌려준다() {
        let req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2024-11-05" }
        });
        let res = handle(&req).unwrap();

        assert_eq!(res["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(res["result"]["serverInfo"]["name"], "drive-archive");
        assert!(res["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize에_버전이_없으면_기본값을_쓴다() {
        let res = handle(&request("initialize", 1)).unwrap();
        assert_eq!(res["result"]["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn 알림에는_응답하지_않는다() {
        let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&notification).is_none());
    }

    #[test]
    fn 도구_목록에_세_가지가_들어_있다() {
        let res = handle(&request("tools/list", 2)).unwrap();
        let tools = res["result"]["tools"].as_array().unwrap();

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["search_files", "list_drives", "get_status"]);
    }

    #[test]
    fn 모든_도구가_입력_스키마를_갖는다() {
        let res = handle(&request("tools/list", 2)).unwrap();
        for tool in res["result"]["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
            assert!(!tool["description"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn search_files는_keyword를_필수로_받는다() {
        let schema = &tool_definitions()[0]["inputSchema"];
        assert_eq!(schema["required"], json!(["keyword"]));
    }

    #[test]
    fn ping에는_빈_결과로_답한다() {
        let res = handle(&request("ping", 3)).unwrap();
        assert_eq!(res["result"], json!({}));
    }

    #[test]
    fn 모르는_메서드는_오류로_답한다() {
        let res = handle(&request("resources/list", 4)).unwrap();
        assert_eq!(res["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(res["id"], 4);
    }

    #[test]
    fn 도구_이름이_없으면_잘못된_인자_오류다() {
        let req = json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "arguments": {} }
        });
        let res = handle(&req).unwrap();
        assert_eq!(res["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn 없는_도구를_부르면_프로토콜_오류가_아니라_is_error다() {
        let req = json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "없는도구", "arguments": {} }
        });
        let res = handle(&req).unwrap();

        // 모델이 읽고 판단할 수 있도록 정상 응답 안에 오류를 담는다.
        assert!(res.get("error").is_none());
        assert_eq!(res["result"]["isError"], true);
    }

    #[test]
    fn 응답에_요청_id가_그대로_실린다() {
        let res = handle(&request("tools/list", 99)).unwrap();
        assert_eq!(res["id"], 99);
        assert_eq!(res["jsonrpc"], "2.0");
    }
}
