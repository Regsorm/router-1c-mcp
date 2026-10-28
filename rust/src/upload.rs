//! Загрузка артефактов через роутер: `POST /upload/{base}`.
//!
//! Маршрут не закрывается белым списком инструментов: клиент получает адрес из
//! подсказки `import_artifact`. Роутер подставляет авторизацию и передаёт тело
//! в HTTP-сервис 1С потоком. Пермит базы держится до конца передачи ответа.

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use futures_util::StreamExt;
use rmcp::model::CallToolResult;
use serde::Deserialize;
use tracing::{info, warn};

use crate::download::{
    encode_component, is_html, patch_first_text, read_body_head, text_error, DownloadState,
};

pub struct UploadTarget<'a> {
    pub id: Option<&'a str>,
    pub filename: Option<&'a str>,
    pub sha_prev: Option<&'a str>,
    pub dryrun: bool,
}

pub fn forward_url(service_base: &str, t: &UploadTarget<'_>) -> String {
    let mut url = format!("{service_base}/?upload=1&kind=additional");
    for (key, value) in [
        ("id", t.id),
        ("filename", t.filename),
        ("sha_prev", t.sha_prev),
    ] {
        if let Some(value) = value {
            url.push_str(&format!("&{key}={}", encode_component(value)));
        }
    }
    if t.dryrun {
        url.push_str("&dryrun=1");
    }
    url
}

#[derive(Deserialize)]
pub struct UploadQuery {
    kind: Option<String>,
    id: Option<String>,
    filename: Option<String>,
    sha_prev: Option<String>,
    dryrun: Option<String>,
}

pub fn routes(state: DownloadState) -> Router {
    Router::new()
        .route("/upload/{base}", post(upload))
        .with_state(state)
}

pub async fn upload(
    State(state): State<DownloadState>,
    Path(base): Path<String>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(fwd) = state.bases.get(&base) else {
        let allowed = state.order.as_ref();
        let msg = format!("Неизвестная база '{base}'. Допустимые: {allowed:?}");
        warn!(base = %base, "запрос на загрузку с неизвестной базой");
        return text_error(StatusCode::NOT_FOUND, msg);
    };

    let kind = q
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("additional");
    if !kind.eq_ignore_ascii_case("additional") {
        warn!(base = %base, kind = %kind, "некорректный kind при загрузке");
        return text_error(
            StatusCode::BAD_REQUEST,
            format!("загрузка возможна только для kind=additional (получено {kind})"),
        );
    }
    let id = q.id.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let filename = q
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let sha_prev = q
        .sha_prev
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let dryrun = q.dryrun.is_some();
    if id.is_none() && filename.is_none() {
        warn!(base = %base, kind = "additional", id = ?id, filename = ?filename, dryrun,
            "нет имени файла при загрузке");
        return text_error(
            StatusCode::BAD_REQUEST,
            "без id обязателен параметр filename".to_string(),
        );
    }
    let target = UploadTarget {
        id,
        filename,
        sha_prev,
        dryrun,
    };
    let request_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let permit = match fwd.acquire().await {
        Ok(p) => p,
        Err(e) => {
            warn!(base = %base, kind = "additional", id = ?id, filename = ?filename,
                dryrun, request_length = ?request_length, error = %e,
                "семафор базы недоступен");
            return text_error(StatusCode::BAD_GATEWAY, format!("Загрузка не удалась: {e}"));
        }
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| {
            "application/octet-stream"
                .parse()
                .expect("статический Content-Type")
        });
    let mut rb = fwd
        .upload_request(&target, state.timeout)
        .header(header::CONTENT_TYPE, content_type)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()));
    if let Some(length) = request_length {
        rb = rb.header(header::CONTENT_LENGTH, length.to_string());
    }
    // `without_url`: адрес базы не должен попасть ни клиенту, ни в журнал.
    let resp = match rb.send().await.map_err(reqwest::Error::without_url) {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            warn!(base = %base, kind = "additional", id = ?id, filename = ?filename,
                dryrun, request_length = ?request_length, error = %e, "таймаут загрузки");
            let secs = state.timeout.as_secs();
            return text_error(
                StatusCode::GATEWAY_TIMEOUT,
                format!("Таймаут загрузки в базу '{base}' ({secs} с): {e}"),
            );
        }
        Err(e) => {
            warn!(base = %base, kind = "additional", id = ?id, filename = ?filename,
                dryrun, request_length = ?request_length, error = %e, "ошибка запроса к базе");
            return text_error(StatusCode::BAD_GATEWAY, format!("Загрузка не удалась: {e}"));
        }
    };
    let status = resp.status();
    let response_length = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("неизвестен")
        .to_string();
    if is_html(&resp) {
        let head = read_body_head(resp).await;
        warn!(base = %base, kind = "additional", id = ?id, filename = ?filename,
            dryrun, status = status.as_u16(), request_length = ?request_length,
            response_length = %response_length, "1С вернула HTML вместо ответа");
        return text_error(StatusCode::BAD_GATEWAY,
            format!("1С вернула HTML вместо ответа (HTTP {status}) — запрос не попал в HTTP-сервис: {head}"));
    }
    info!(base = %base, kind = "additional", id = ?id, filename = ?filename,
        dryrun, status = status.as_u16(), request_length = ?request_length,
        response_length = %response_length, "загрузка артефакта");
    let mut builder = Response::builder().status(status);
    for name in [header::CONTENT_TYPE, header::CONTENT_LENGTH] {
        if let Some(value) = resp.headers().get(&name) {
            builder = builder.header(name, value);
        }
    }
    let stream = resp.bytes_stream().map(move |chunk| {
        let _keep = &permit;
        chunk
    });
    match builder.body(Body::from_stream(stream)) {
        Ok(r) => r,
        Err(e) => {
            warn!(base = %base, kind = "additional", id = ?id, filename = ?filename,
                dryrun, status = status.as_u16(), request_length = ?request_length,
                response_length = %response_length, error = %e, "не удалось собрать ответ");
            text_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Не удалось собрать ответ загрузки.".to_string(),
            )
        }
    }
}

pub fn rewrite_import_hint(text: &str, public: &str, base: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let marker = lines
        .iter()
        .position(|line| line.trim().starts_with("Загрузка: к адресу MCP-эндпоинта"))?;
    let query = lines.get(marker + 1)?.trim_start();
    if !query.starts_with("?upload=1&") {
        return None;
    }
    let query = query.trim_end().trim_start_matches('?');
    let known = [
        "upload=",
        "kind=",
        "id=",
        "filename=",
        "sha_prev=",
        "dryrun=",
    ];
    let mut pairs = Vec::new();
    let mut start = 0;
    for (index, _) in query.char_indices().filter(|(_, c)| *c == '&') {
        if known.iter().any(|key| query[index + 1..].starts_with(key)) {
            pairs.push(&query[start..index]);
            start = index + 1;
        }
    }
    pairs.push(&query[start..]);
    let params: Vec<String> = pairs
        .into_iter()
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            if key == "upload" && value == "1" {
                return None;
            }
            let encoded = if value.starts_with('<') && value.ends_with('>') {
                value.to_string()
            } else {
                encode_component(value)
            };
            Some(format!("{key}={encoded}"))
        })
        .collect();
    let address = format!(
        "Загрузка: POST {public}/upload/{}?{}",
        encode_component(base),
        params.join("&")
    );
    let mut out = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        if index == marker {
            out.push(address.clone());
        } else if index == marker + 1 {
            continue;
        } else if line
            .trim_start()
            .starts_with("Метод POST, тело запроса — содержимое файла целиком")
        {
            out.push("Учётные данные не нужны — их подставляет роутер. Тело запроса — содержимое файла целиком.".to_string());
        } else {
            out.push((*line).to_string());
        }
    }
    Some(out.join("\n"))
}

pub fn patch_import_result(res: CallToolResult, public: &str, base: &str) -> CallToolResult {
    patch_first_text(res, |text| rewrite_import_hint(text, public, base))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::{Body, Bytes};
    use axum::extract::{Path, Query, State};
    use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
    use axum::response::Response;
    use axum::routing::post;
    use axum::Router;
    use rmcp::model::CallToolResult;
    use serde_json::json;

    use super::{
        forward_url, patch_import_result, rewrite_import_hint, routes, upload, UploadQuery,
        UploadTarget,
    };
    use crate::config::BaseEntry;
    use crate::download::{encode_component, DownloadState};
    use crate::forward::BaseForwarder;

    /// Пароль поддельной 1С: вместе с `user` даёт `Basic dXNlcjpwYXNz`.
    const ПАРОЛЬ_ФЕЙКА: &str = "pass";

    fn база(name: &str, url: &str, max_concurrent: Option<usize>) -> BaseEntry {
        serde_json::from_value(json!({
            "name": name,
            "url": url,
            "username": "user",
            "password": ПАРОЛЬ_ФЕЙКА,
            "service_root": "mcp",
            "max_concurrent": max_concurrent,
        }))
        .expect("валидное описание базы")
    }

    fn состояние(entries: &[BaseEntry], timeout_secs: u64) -> DownloadState {
        let client = reqwest::Client::new();
        let mut bases = HashMap::new();
        let mut order = Vec::new();
        for entry in entries {
            bases.insert(
                entry.name.clone(),
                BaseForwarder::new(entry, client.clone()),
            );
            order.push(entry.name.clone());
        }
        DownloadState::new(
            Arc::new(bases),
            Arc::new(order),
            Duration::from_secs(timeout_secs),
        )
    }

    fn заголовок(headers: &HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    async fn тело_ответа(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("тело ответа");
        String::from_utf8_lossy(&bytes).to_string()
    }

    fn результат(text: &str, is_error: bool) -> CallToolResult {
        serde_json::from_value(json!({
            "content": [{"type": "text", "text": text}], "isError": is_error,
        }))
        .expect("валидная форма CallToolResult")
    }

    fn текст_результата(res: &CallToolResult) -> String {
        serde_json::to_value(res).expect("сериализация CallToolResult")["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn параметры(
        kind: Option<&str>,
        id: Option<&str>,
        filename: Option<&str>,
    ) -> Query<UploadQuery> {
        Query(UploadQuery {
            kind: kind.map(str::to_string),
            id: id.map(str::to_string),
            filename: filename.map(str::to_string),
            sha_prev: None,
            dryrun: None,
        })
    }

    #[tokio::test]
    async fn плохие_параметры_отбиваются_до_сети() {
        let st = состояние(&[база("b", "http://127.0.0.1:1/base", None)], 5);
        for query in [
            параметры(Some("extension"), Some("x"), None),
            параметры(None, None, None),
            параметры(None, Some("  "), None),
        ] {
            let resp = upload(
                State(st.clone()),
                Path("b".to_string()),
                query,
                HeaderMap::new(),
                Body::empty(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn неизвестная_база_даёт_404() {
        let st = состояние(&[база("allowed", "http://127.0.0.1:1/base", None)], 5);
        let resp = upload(
            State(st),
            Path("missing".to_string()),
            параметры(None, None, Some("x.epf")),
            HeaderMap::new(),
            Body::empty(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(тело_ответа(resp).await.contains("allowed"));
    }

    #[test]
    fn адрес_пересылки() {
        let target = UploadTarget {
            id: None,
            filename: Some("Отчёт (1).epf"),
            sha_prev: None,
            dryrun: true,
        };
        assert_eq!(forward_url("http://host/base/hs/mcp", &target),
            "http://host/base/hs/mcp/?upload=1&kind=additional&filename=%D0%9E%D1%82%D1%87%D1%91%D1%82%20%281%29.epf&dryrun=1");
    }

    #[test]
    fn подсказка_с_id() {
        let source = "Артефакт найден.\nСейчас в базе: Отчёт.epf\nSHA-256: abc\n\
            Загрузка: к адресу MCP-эндпоинта добавьте параметры:\n\
            ?upload=1&kind=additional&id=Отчёт&filename=Свод & итог.epf&sha_prev=старый\n\
            Метод POST, тело запроса — содержимое файла целиком.\n\
            Для защиты используйте sha_prev.\nДобавьте &dryrun=1 для проверки.";
        let text = rewrite_import_hint(source, "http://router:8014", "base-a")
            .expect("подсказка меняется");
        assert!(text.contains(&format!(
            "Загрузка: POST http://router:8014/upload/base-a?kind=additional&id={}&filename={}&sha_prev={}",
            encode_component("Отчёт"), encode_component("Свод & итог.epf"),
            encode_component("старый"))), "{text}");
        assert!(!text.contains("?upload=1"));
        assert!(!text.contains("те же учётные данные"));
        for part in ["Сейчас в базе:", "SHA-256:", "sha_prev", "&dryrun=1"] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(
            !text.contains("X-MCP-Key"),
            "заголовка ключа в подсказке нет: {text}"
        );
    }

    #[test]
    fn подсказка_без_id() {
        let source = "Загрузка: к адресу MCP-эндпоинта добавьте параметры:\n\
            ?upload=1&kind=additional&filename=<имя файла с расширением>\n\
            Имя файла обязательно, если id не указан.";
        let text = rewrite_import_hint(source, "http://router:8014", "base-a")
            .expect("подсказка меняется");
        assert!(text.contains("/upload/base-a?kind=additional&filename=<имя файла с расширением>"));
        assert!(text.contains("Имя файла обязательно"));
    }

    #[test]
    fn ошибка_инструмента_не_меняется() {
        let source = "Загрузка: к адресу MCP-эндпоинта\n?upload=1&kind=additional&filename=x.epf";
        let res = patch_import_result(результат(source, true), "http://router", "base-a");
        assert_eq!(текст_результата(&res), source);
    }

    #[test]
    fn текст_без_маркера_не_меняется() {
        let source = "Артефакт не найден";
        let res = patch_import_result(результат(source, false), "http://router", "base-a");
        assert_eq!(текст_результата(&res), source);
    }

    #[derive(Clone, Debug)]
    struct Запрос1С {
        authorization: String,
        has_mcp_key: bool,
        query: HashMap<String, String>,
        content_length: String,
        transfer_encoding: String,
        body: Vec<u8>,
    }

    #[derive(Clone)]
    struct Фейковая1С(Arc<Mutex<Vec<Запрос1С>>>);

    fn ответ(status: StatusCode, body: &'static str, content_type: &'static str) -> Response {
        let mut resp = Response::new(Body::from(body));
        *resp.status_mut() = status;
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        resp.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&body.len().to_string()).expect("длина ответа"),
        );
        resp
    }

    async fn фейковая_1с(
        State(st): State<Фейковая1С>,
        Query(query): Query<HashMap<String, String>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        st.0.lock().expect("журнал запросов").push(Запрос1С {
            authorization: заголовок(&headers, header::AUTHORIZATION.as_str()),
            // Присутствие заголовка, а не его значение: пустой X-MCP-Key — тоже ключ.
            has_mcp_key: headers.contains_key("X-MCP-Key"),
            query: query.clone(),
            content_length: заголовок(&headers, header::CONTENT_LENGTH.as_str()),
            transfer_encoding: заголовок(&headers, header::TRANSFER_ENCODING.as_str()),
            body: body.to_vec(),
        });
        match query.get("filename").map(String::as_str) {
            Some("bad.epf") => ответ(
                StatusCode::BAD_REQUEST,
                r#"{"error":"bad"}"#,
                "application/json",
            ),
            Some("page.epf") => ответ(
                StatusCode::OK,
                "<html>wrong service</html>",
                "text/html; charset=utf-8",
            ),
            _ => ответ(StatusCode::OK, r#"{"result":"ok"}"#, "application/json"),
        }
    }

    async fn поднять_фейк(st: Фейковая1С) -> String {
        let app = Router::new()
            .route("/base/hs/mcp/", post(фейковая_1с))
            // Лимит тела снят у поддельной 1С: проверяется только роутер.
            .layer(axum::extract::DefaultBodyLimit::disable())
            .with_state(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind поддельной 1С");
        let addr = listener.local_addr().expect("адрес поддельной 1С");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/base")
    }

    async fn поднять_роутер(st: DownloadState) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind роутера");
        let addr = listener.local_addr().expect("адрес роутера");
        tokio::spawn(async move {
            let _ = axum::serve(listener, routes(st)).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn сквозная_загрузка_через_роутер() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base_url = поднять_фейк(Фейковая1С(seen.clone())).await;
        let st = состояние(&[база("mybase", &base_url, Some(1))], 30);
        let router = поднять_роутер(st).await;
        let client = reqwest::Client::new();
        // Больше 2 МБ — стандартного лимита тела у axum: он не должен срабатывать.
        let файл: Vec<u8> = (0..3 * 1024 * 1024)
            .map(|i| ((i * 31 + 7) % 251) as u8)
            .collect();
        let target = UploadTarget {
            id: Some("Отчёт"),
            filename: Some("Отчёт (1).epf"),
            sha_prev: Some("старый"),
            dryrun: true,
        };
        let url = format!(
            "{router}/upload/mybase?kind=additional&id={}&filename={}&sha_prev={}&dryrun=1",
            encode_component(target.id.expect("id")),
            encode_component(target.filename.expect("filename")),
            encode_component(target.sha_prev.expect("sha_prev")),
        );
        let resp = client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, файл.len().to_string())
            .body(файл.clone())
            .send()
            .await
            .expect("загрузка файла");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.expect("ответ 1С"), r#"{"result":"ok"}"#);
        {
            let seen = seen.lock().expect("журнал запросов");
            let first = &seen[0];
            assert_eq!(first.authorization, "Basic dXNlcjpwYXNz");
            assert!(!first.has_mcp_key, "роутер не отправляет X-MCP-Key в 1С");
            for (key, value) in [
                ("upload", "1"),
                ("kind", "additional"),
                ("id", "Отчёт"),
                ("filename", "Отчёт (1).epf"),
                ("sha_prev", "старый"),
                ("dryrun", "1"),
            ] {
                assert_eq!(first.query.get(key).map(String::as_str), Some(value));
            }
            assert_eq!(first.content_length, файл.len().to_string());
            assert!(first.transfer_encoding.is_empty());
            assert_eq!(first.body, файл);
        }

        let url = format!("{router}/upload/mybase?filename=bad.epf");
        let resp = client
            .post(&url)
            .body("bad")
            .send()
            .await
            .expect("неверный файл");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.text().await.expect("ошибка 1С"), r#"{"error":"bad"}"#);

        let url = format!("{router}/upload/mybase?filename=page.epf");
        let resp = client
            .post(&url)
            .body("page")
            .send()
            .await
            .expect("HTML 1С");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let text = resp.text().await.expect("ошибка HTML");
        assert!(
            text.contains("HTTP 200") && text.contains("<html>"),
            "{text}"
        );

        let resp = client
            .get(format!("{router}/upload/mybase"))
            .send()
            .await
            .expect("GET на маршрут загрузки");
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let url = format!("{router}/upload/mybase?filename=again.epf");
        for _ in 0..2 {
            let resp = tokio::time::timeout(
                Duration::from_secs(10),
                client.post(&url).body("next").send(),
            )
            .await
            .expect("пермит не завис")
            .expect("загрузка дошла");
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(resp.text().await.expect("ответ 1С"), r#"{"result":"ok"}"#);
        }

        // Пустое значение X-MCP-Key — тоже присутствие заголовка: фикстура
        // фиксирует его через contains_key и видит такой запрос.
        let resp = client
            .post(format!(
                "{base_url}/hs/mcp/?upload=1&kind=additional&filename=empty-key.epf"
            ))
            .header("X-MCP-Key", "")
            .body("empty")
            .send()
            .await
            .expect("прямой запрос к поддельной 1С");
        assert_eq!(resp.status(), StatusCode::OK);
        {
            let seen = seen.lock().expect("журнал запросов");
            let last = seen.last().expect("журнал не пуст");
            assert!(
                last.has_mcp_key,
                "пустой X-MCP-Key тоже считается присутствием заголовка"
            );
        }
    }
}
