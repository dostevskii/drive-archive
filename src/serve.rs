//! 브라우저로 인덱스를 들여다보는 로컬 웹 서버.
//!
//! `mcp.rs`와 같은 이유로 웹 프레임워크를 쓰지 않는다. 필요한 것은
//! 로컬호스트에서 몇 개의 GET 요청에 답하는 것뿐이고, 그 정도는 `std::net`으로 된다.
//! 화면(HTML/CSS/JS)과 폰트는 실행 파일에 넣어 두므로 배포할 파일이 exe 하나로 끝난다.
//!
//! 127.0.0.1에만 바인딩한다. 인덱스에는 사용자의 파일 이름과 경로가 통째로 들어 있어서,
//! 같은 네트워크의 다른 기기에 열어 줄 이유가 없다.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

use anyhow::{Context, Result};

/// 화면. 한 파일에 HTML·CSS·JS가 모두 들어 있다.
const INDEX_HTML: &str = include_str!("web/index.html");

/// 화면에 쓰는 고정폭 폰트 (FiraD2 Regular).
const FONT_WOFF2: &[u8] = include_bytes!("../assets/FiraD2-Regular.woff2");

/// 검색 결과 상한. 화면이 감당할 수 있는 만큼만 보낸다.
const MAX_LIMIT: usize = 500;

/// 폴더 하나를 펼칠 때의 상한. 사진 폴더처럼 한 곳에 수천 개가 있는 경우가 있어
/// 검색보다 넉넉하게 잡는다.
const MAX_BROWSE: usize = 2000;

/// 서버를 띄우고 요청을 계속 받는다. Ctrl+C로 끝낸다.
pub fn serve(port: u16, open_browser: bool) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("{port}번 포트를 열 수 없습니다 (이미 쓰고 있는지 확인하세요)"))?;

    let url = format!("http://127.0.0.1:{port}/");
    println!("drive-archive 웹 화면: {url}");
    println!("끝내려면 Ctrl+C를 누르세요.");

    if open_browser {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn();
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // 브라우저는 페이지·폰트·API를 동시에 요청한다. 요청마다 스레드를 띄워
        // 하나가 느려도 나머지가 기다리지 않게 한다.
        std::thread::spawn(move || {
            if let Err(e) = handle(stream) {
                eprintln!("요청 처리 실패: {e:#}");
            }
        });
    }
    Ok(())
}

/// 연결 하나를 처리한다.
fn handle(mut stream: TcpStream) -> Result<()> {
    let target = match read_request_target(&mut stream)? {
        Some(t) => t,
        None => return Ok(()),
    };

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target.as_str(), ""),
    };

    match path {
        "/" | "/index.html" => {
            respond(&mut stream, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes())
        }
        "/font.woff2" => respond(&mut stream, 200, "font/woff2", FONT_WOFF2),
        "/api/drives" => match api_drives() {
            Ok(body) => respond(&mut stream, 200, "application/json; charset=utf-8", body.as_bytes()),
            Err(e) => respond_error(&mut stream, &e),
        },
        "/api/search" => match api_search(query) {
            Ok(body) => respond(&mut stream, 200, "application/json; charset=utf-8", body.as_bytes()),
            Err(e) => respond_error(&mut stream, &e),
        },
        "/api/list" => match api_list(query) {
            Ok(body) => respond(&mut stream, 200, "application/json; charset=utf-8", body.as_bytes()),
            Err(e) => respond_error(&mut stream, &e),
        },
        _ => respond(&mut stream, 404, "text/plain; charset=utf-8", b"not found"),
    }
}

/// 요청 줄에서 경로를 꺼낸다. GET이 아니면 `None`을 준다.
///
/// 헤더는 쓰지 않지만, 읽지 않고 끊으면 브라우저가 연결 오류로 보므로 끝까지 읽어 버린다.
fn read_request_target(stream: &mut TcpStream) -> Result<Option<String>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }

    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/").to_string();

    // 헤더를 빈 줄까지 흘려보낸다.
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }

    if method != "GET" {
        return Ok(None);
    }
    Ok(Some(target))
}

/// 하드 목록을 JSON으로 만든다.
fn api_drives() -> Result<String> {
    let conn = crate::db::open()?;
    let drives = crate::db::list_drives(&conn)?;
    let stats = crate::db::stats(&conn)?;
    Ok(serde_json::json!({ "drives": drives, "stats": stats }).to_string())
}

/// 검색 결과를 JSON으로 만든다.
fn api_search(query: &str) -> Result<String> {
    let mut keyword = String::new();
    let mut dirs_only = false;
    let mut limit = 200usize;

    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "q" => keyword = percent_decode(v),
            "dirs" => dirs_only = v == "1",
            "limit" => limit = v.parse().unwrap_or(limit),
            _ => {}
        }
    }

    let keyword = keyword.trim().to_string();
    if keyword.is_empty() {
        return Ok(serde_json::json!({ "query": "", "hits": [] }).to_string());
    }

    // 한 개 더 받아 본다. 더 나오면 상한에 걸린 것이고, 화면은 그 사실을
    // `200+`처럼 알려야 한다. 상한을 정확한 건수로 보여 주면 거짓말이 된다.
    let conn = crate::db::open()?;
    let want = limit.min(MAX_LIMIT);
    let mut hits = crate::db::search(&conn, &keyword, None, dirs_only, want + 1)?;
    let truncated = hits.len() > want;
    hits.truncate(want);

    Ok(serde_json::json!({
        "query": keyword,
        "hits": hits,
        "truncated": truncated,
    })
    .to_string())
}

/// 폴더 하나의 바로 아래 항목을 JSON으로 만든다.
///
/// 하드를 연결하지 않아도 인덱스에 남은 내용을 그대로 훑어볼 수 있다.
fn api_list(query: &str) -> Result<String> {
    let mut serial = String::new();
    let mut dir = String::new();
    let mut limit = MAX_BROWSE;

    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "serial" => serial = percent_decode(v),
            "dir" => dir = percent_decode(v),
            "limit" => limit = v.parse().unwrap_or(limit),
            _ => {}
        }
    }

    if serial.is_empty() {
        anyhow::bail!("어느 하드인지 지정되지 않았습니다");
    }

    let conn = crate::db::open()?;
    let want = limit.min(MAX_BROWSE);
    let mut items = crate::db::list_children(&conn, &serial, &dir, want + 1)?;
    let truncated = items.len() > want;
    items.truncate(want);

    Ok(serde_json::json!({
        "serial": serial,
        "dir": dir,
        "items": items,
        "truncated": truncated,
    })
    .to_string())
}

/// 쿼리 문자열의 퍼센트 인코딩을 푼다. `+`는 공백으로 본다.
///
/// 한글 검색어가 `%ED%95%9C` 꼴로 오므로 바이트로 모아 UTF-8로 되돌린다.
fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }

    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            // 바이트로 읽는다. `%` 뒤에 멀티바이트 글자가 그대로 온 경우
            // (`%한` 같은 것) 문자열을 잘라 보려 하면 글자 중간에서 깨진다.
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 응답 하나를 보낸다.
///
/// keep-alive를 쓰지 않는다. 연결을 닫으면 브라우저가 알아서 다시 여는데,
/// 로컬에서는 그 비용이 없는 것이나 마찬가지이고 서버 쪽이 훨씬 단순해진다.
fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

/// 조회에 실패했을 때, 화면이 사유를 띄울 수 있도록 JSON으로 알린다.
fn respond_error(stream: &mut TcpStream, e: &anyhow::Error) -> Result<()> {
    let body = serde_json::json!({ "error": format!("{e:#}") }).to_string();
    respond(stream, 500, "application/json; charset=utf-8", body.as_bytes())
}

/// 응답을 통째로 읽는다. 테스트에서만 쓴다.
#[cfg(test)]
fn read_all(mut stream: TcpStream) -> String {
    use std::io::Read;
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 퍼센트_인코딩을_푼다() {
        assert_eq!(percent_decode("hello"), "hello");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%ED%95%9C%EA%B8%80"), "한글");
        assert_eq!(percent_decode("Works%20D"), "Works D");
    }

    #[test]
    fn 잘린_인코딩은_글자_그대로_둔다() {
        // 사용자가 검색어에 `%`를 그냥 친 경우다. 깨지지 않고 살아남아야 한다.
        assert_eq!(percent_decode("50%"), "50%");
        assert_eq!(percent_decode("%zz"), "%zz");
        // `%` 뒤에 인코딩되지 않은 한글이 그대로 온 경우. 글자 중간에서 잘리면 안 된다.
        assert_eq!(percent_decode("%한글"), "%한글");
    }

    #[test]
    fn 빈_검색어는_결과가_없다() {
        let body = api_search("q=").unwrap();
        assert!(body.contains(r#""hits":[]"#), "{body}");
    }

    #[test]
    fn 검색어의_앞뒤_공백은_무시한다() {
        let body = api_search("q=%20%20").unwrap();
        assert!(body.contains(r#""hits":[]"#), "{body}");
    }

    /// 실제로 포트를 열어 요청-응답이 오가는지 확인한다.
    #[test]
    fn 페이지와_폰트를_내려준다() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in listener.incoming().take(3) {
                let _ = handle(s.unwrap());
            }
        });

        let get = |path: &str| -> String {
            let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .unwrap();
            read_all(s)
        };

        let page = get("/");
        assert!(page.starts_with("HTTP/1.1 200 OK"), "{}", &page[..60.min(page.len())]);
        assert!(page.contains("DRIVE-ARCHIVE"));

        let font = get("/font.woff2");
        assert!(font.starts_with("HTTP/1.1 200 OK"));
        assert!(font.contains("font/woff2"));

        let missing = get("/없는경로");
        assert!(missing.starts_with("HTTP/1.1 404"));
    }
}
