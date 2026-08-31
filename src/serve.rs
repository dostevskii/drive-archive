//! 브라우저로 인덱스를 들여다보는 로컬 웹 서버.
//!
//! `mcp.rs`와 같은 이유로 웹 프레임워크를 쓰지 않는다. 필요한 것은
//! 로컬호스트에서 몇 개의 GET 요청에 답하는 것뿐이고, 그 정도는 `std::net`으로 된다.
//! 화면(HTML/CSS/JS)과 폰트는 실행 파일에 넣어 두므로 배포할 파일이 exe 하나로 끝난다.
//!
//! 127.0.0.1에만 바인딩한다. 인덱스에는 사용자의 파일 이름과 경로가 통째로 들어 있어서,
//! 같은 네트워크의 다른 기기에 열어 줄 이유가 없다.

use std::io::{BufRead, BufReader, Read, Write};
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
    let peer = stream
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "?".to_string());
    let Some(req) = read_request(&mut stream, peer)? else {
        return Ok(());
    };
    if req.method != "GET" {
        return respond(&mut stream, 404, "text/plain; charset=utf-8", b"not found");
    }
    let (path, query) = (req.path.as_str(), req.query.as_str());

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

/// 받아 줄 본문의 최대 크기. 로그인 본문은 몇십 바이트라 넉넉히 잡아도 이 정도다.
const MAX_BODY: usize = 8192;

/// 받아 줄 헤더 전체의 최대 크기. 브라우저 요청은 1KB 안팎이다.
const MAX_HEADERS: usize = 16 * 1024;

/// 요청 하나.
///
/// 헤더 이름은 소문자로 눕혀 보관한다. HTTP 헤더 이름은 대소문자를 가리지 않는데
/// 프록시마다 다르게 적어 보낸다.
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// 소켓 상대 주소. 터널 뒤에서는 전부 127.0.0.1로 보인다.
    peer: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// 쿠키 하나를 이름으로 꺼낸다.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        let raw = self.header("cookie")?;
        raw.split(';').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k.trim() == name).then(|| v.trim())
        })
    }

    /// 로그인 실패를 셀 때 쓸 주소.
    ///
    /// 터널을 거치면 소켓 주소가 전부 127.0.0.1이 되어, 그것만 쓰면 한 사람의 오타가
    /// 전체를 잠근다. 다만 이 헤더는 보내는 쪽이 지어낼 수 있으므로 이것만 믿지
    /// 않는다 — `auth::Gate`가 전역 카운터를 함께 센다.
    pub fn client_ip(&self) -> String {
        self.header("x-forwarded-for")
            .and_then(|v| v.split(',').next())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.peer.clone())
    }

    /// 터널이 HTTPS로 받아 넘긴 요청인가. 쿠키에 `Secure`를 붙일지 정하는 데 쓴다.
    pub fn is_https(&self) -> bool {
        self.header("x-forwarded-proto").is_some_and(|v| v.eq_ignore_ascii_case("https"))
    }
}

/// 요청 하나를 끝까지 읽는다. 읽을 수 없는 요청이면 `None`을 준다.
fn read_request(stream: &mut TcpStream, peer: String) -> Result<Option<Request>> {
    // 터널을 거쳐 아무나 닿는 자리다. 한 글자씩 흘리며 스레드를 영영 붙드는
    // 연결은 시간으로 끊고, 전체 요청은 크기로 자른다. `take`가 헤더 한 줄이
    // 한없이 자라는 것까지 막는다.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let mut reader = BufReader::new(stream.try_clone()?.take(64 * 1024));

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut headers = Vec::new();
    let mut header_bytes = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        header_bytes += h.len();
        if header_bytes > MAX_HEADERS {
            return Ok(None);
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Ok(None);
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body)?;
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    Ok(Some(Request { method, path, query, headers, body, peer }))
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

    /// 테스트용 요청을 하나 만든다.
    fn req(raw: &str) -> Option<Request> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let raw = raw.to_string();
        std::thread::spawn(move || {
            let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            let _ = s.write_all(raw.as_bytes());
            let _ = s.flush();
            // 서버가 읽을 때까지 붙들고 있는다. 먼저 닫으면 본문이 잘린다.
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream, "1.2.3.4".to_string()).unwrap()
    }

    #[test]
    fn 경로와_쿼리를_나눠_읽는다() {
        let r = req("GET /api/search?q=%ED%95%9C HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/api/search");
        assert_eq!(r.query, "q=%ED%95%9C");
    }

    #[test]
    fn 쿼리가_없으면_빈_문자열이다() {
        let r = req("GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(r.path, "/");
        assert_eq!(r.query, "");
    }

    #[test]
    fn 헤더_이름은_대소문자를_가리지_않는다() {
        let r = req("GET / HTTP/1.1\r\nX-Forwarded-Proto: https\r\n\r\n").unwrap();
        assert_eq!(r.header("x-forwarded-proto"), Some("https"));
        assert_eq!(r.header("X-Forwarded-Proto"), Some("https"));
        assert!(r.is_https());
    }

    #[test]
    fn 프로토콜_헤더가_없으면_평문으로_본다() {
        let r = req("GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert!(!r.is_https());
    }

    #[test]
    fn 쿠키를_이름으로_꺼낸다() {
        let r = req("GET / HTTP/1.1\r\nCookie: other=1; da=abc123; last=z\r\n\r\n").unwrap();
        assert_eq!(r.cookie("da"), Some("abc123"));
        assert_eq!(r.cookie("other"), Some("1"));
        assert_eq!(r.cookie("last"), Some("z"));
        assert_eq!(r.cookie("없는것"), None);
    }

    #[test]
    fn 쿠키가_없어도_터지지_않는다() {
        let r = req("GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(r.cookie("da"), None);
        let r = req("GET / HTTP/1.1\r\nCookie: \r\n\r\n").unwrap();
        assert_eq!(r.cookie("da"), None);
    }

    #[test]
    fn 포스트_본문을_읽는다() {
        let body = r#"{"password":"열려라참깨"}"#;
        let raw = format!(
            "POST /api/login HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let r = req(&raw).unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(String::from_utf8_lossy(&r.body), body);
    }

    #[test]
    fn 본문이_너무_크면_받지_않는다() {
        // 로그인 본문은 몇십 바이트다. 큰 것을 받아 줄 이유가 없다.
        let raw = format!(
            "POST /api/login HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(req(&raw).is_none());
    }

    #[test]
    fn 헤더를_한없이_받아_주지_않는다() {
        // 터널을 거쳐 아무나 보낼 수 있는 자리다. 헤더를 끝없이 흘리면
        // 요청마다 메모리가 그만큼 자란다.
        let raw = format!("GET / HTTP/1.1\r\n{}\r\n", "X-Filler: 채움\r\n".repeat(2000));
        assert!(req(&raw).is_none());
    }

    #[test]
    fn 전달받은_주소를_쓰고_없으면_소켓_주소를_쓴다() {
        let r = req("GET / HTTP/1.1\r\nX-Forwarded-For: 9.9.9.9, 8.8.8.8\r\n\r\n").unwrap();
        assert_eq!(r.client_ip(), "9.9.9.9", "맨 앞이 원래 보낸 쪽이다");
        let r = req("GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(r.client_ip(), "1.2.3.4");
    }
}
