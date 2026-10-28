//! Скачивание артефактов через роутер: `GET /download/{base}?kind=..&id=..`.
//!
//! Маршрут не закрывается белым списком инструментов: он нужен клиенту, который
//! получил подсказку от `export_artifact`, но по какой-то причине не может
//! звать инструменты 1С. Запрос уходит в тот же HTTP-сервис базы
//! (`{корень сервиса}/?download=1&kind=..&id=..`), учётные данные подставляет
//! роутер, тело ответа 1С передаётся клиенту как есть.
//!
//! Пермит семафора базы (`max_concurrent`) держится до конца передачи файла,
//! а не до получения заголовков ответа.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use futures_util::StreamExt;
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use crate::forward::BaseForwarder;

/// Виды артефактов, которые умеет отдавать HTTP-сервис 1С.
const KINDS: [&str; 3] = ["additional", "supplied", "extension"];

/// Сколько байт HTML-ответа показывать в сообщении об ошибке.
const HTML_SNIPPET_BYTES: usize = 500;

/// Процентное кодирование одного компонента URL (query или path).
/// Свободными остаются только `A-Z a-z 0-9 - . _ ~`; всё прочее (включая
/// пробел и кириллицу) кодируется по байтам UTF-8 в верхнем регистре.
/// Пробел — `%20`, а не `+`: HTTP-сервис 1С читает `+` буквально.
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(*b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Адрес пересылки в HTTP-сервис 1С: корень сервиса (без `/rpc`) со слешем,
/// затем параметры запроса на скачивание.
pub fn forward_url(service_base: &str, kind: &str, id: &str) -> String {
    let kind = encode_component(kind);
    let id = encode_component(id);
    format!("{service_base}/?download=1&kind={kind}&id={id}")
}

/// Внешний адрес скачивания для подсказки клиенту: `{public}/download/{base}?kind=..&id=..`.
pub fn public_download_url(public: &str, base: &str, kind: &str, id: &str) -> String {
    let base = encode_component(base);
    let kind = encode_component(kind);
    let id = encode_component(id);
    format!("{public}/download/{base}?kind={kind}&id={id}")
}

/// Блок подсказки о скачивании. Первая строка — маркер: по нему прежний блок
/// (если он есть в тексте) находится и заменяется целиком.
fn hint_block(url: &str) -> String {
    format!(
        "Скачивание: GET {url}\n\
         Учётные данные не нужны — их подставляет роутер.\n\
         Тело ответа — содержимое файла; после скачивания сверьте SHA-256 с указанным выше."
    )
}

/// Позиция строки, начинающейся с `Скачивание:`, — в начале текста или сразу
/// после перевода строки.
fn download_line_start(text: &str) -> Option<usize> {
    if text.starts_with("Скачивание:") {
        return Some(0);
    }
    text.match_indices("\nСкачивание:")
        .next()
        .map(|(i, _)| i + 1)
}

/// Переписать подсказку `export_artifact` так, чтобы клиент качал артефакт через
/// роутер. `None` — текст менять не нужно (в нём нет `SHA-256:`).
///
/// Если блок «Скачивание:» уже есть, он заменяется целиком (от его начала до
/// конца текста); иначе блок дописывается в конец.
pub fn rewrite_hint(text: &str, url: &str) -> Option<String> {
    if !text.contains("SHA-256:") {
        return None;
    }
    let block = hint_block(url);
    if let Some(pos) = download_line_start(text) {
        let head = &text[..pos];
        return Some(format!("{head}{block}"));
    }
    let head = text.trim_end();
    Some(format!("{head}\n\n{block}"))
}

/// Подменить текст подсказки в результате `export_artifact`. Ошибки инструмента
/// (`isError`) и результаты без `SHA-256:` возвращаются как есть. Работаем через
/// serde — чтобы не зависеть от точных полей структуры rmcp.
pub fn patch_export_result(res: CallToolResult, url: &str) -> CallToolResult {
    patch_first_text(res, |text| rewrite_hint(text, url))
}

/// Подменить первый текстовый элемент результата, для которого есть замена.
pub(crate) fn patch_first_text(
    res: CallToolResult,
    rewrite: impl Fn(&str) -> Option<String>,
) -> CallToolResult {
    let Ok(mut value) = serde_json::to_value(&res) else {
        return res;
    };
    if value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return res;
    }
    let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) else {
        return res;
    };

    let mut patched = false;
    for item in content.iter_mut() {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(new_text) = obj.get("text").and_then(Value::as_str).and_then(&rewrite) else {
            continue;
        };
        obj.insert("text".to_string(), Value::String(new_text));
        patched = true;
        break;
    }
    if !patched {
        return res;
    }
    serde_json::from_value(value).unwrap_or(res)
}

/// Состояние маршрута скачивания: базы роутера и общий таймаут передачи.
#[derive(Clone)]
pub struct DownloadState {
    pub(crate) bases: Arc<HashMap<String, BaseForwarder>>,
    pub(crate) order: Arc<Vec<String>>,
    pub(crate) timeout: Duration,
}

impl DownloadState {
    pub fn new(
        bases: Arc<HashMap<String, BaseForwarder>>,
        order: Arc<Vec<String>>,
        timeout: Duration,
    ) -> Self {
        Self {
            bases,
            order,
            timeout,
        }
    }
}

/// Параметры запроса. Поля `Option`, чтобы на отсутствующий параметр отвечал наш
/// 400 с понятным текстом, а не отказ axum.
#[derive(Deserialize)]
pub struct DownloadQuery {
    kind: Option<String>,
    id: Option<String>,
}

/// Полный роутер маршрута скачивания (`/download/{base}`).
pub fn routes(state: DownloadState) -> Router {
    Router::new()
        .route("/download/{base}", get(download))
        .with_state(state)
}

/// `GET /download/{base}?kind=..&id=..` — скачать артефакт через роутер.
pub async fn download(
    State(state): State<DownloadState>,
    Path(base): Path<String>,
    Query(q): Query<DownloadQuery>,
) -> Response {
    let Some(fwd) = state.bases.get(&base) else {
        let allowed = state.order.as_ref();
        let msg = format!("Неизвестная база '{base}'. Допустимые: {allowed:?}");
        warn!(base = %base, "запрос на скачивание с неизвестной базой");
        return text_error(StatusCode::NOT_FOUND, msg);
    };

    let kind = match q.kind.as_deref() {
        Some(k) if KINDS.contains(&k) => k.to_string(),
        other => {
            let msg = format!(
                "Параметр 'kind' обязателен и должен быть одним из {KINDS:?} (получено {other:?})."
            );
            warn!(base = %base, kind = ?other, "некорректный kind при скачивании");
            return text_error(StatusCode::BAD_REQUEST, msg);
        }
    };

    let id = match q.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => {
            let msg = "Параметр 'id' обязателен и не должен быть пустым.".to_string();
            warn!(base = %base, kind = %kind, "некорректный id при скачивании");
            return text_error(StatusCode::BAD_REQUEST, msg);
        }
    };

    let permit = match fwd.acquire().await {
        Ok(p) => p,
        Err(e) => {
            warn!(base = %base, kind = %kind, id = %id, error = %e, "семафор базы недоступен");
            return text_error(
                StatusCode::BAD_GATEWAY,
                format!("Скачивание не удалось: {e}"),
            );
        }
    };

    // Учётные данные уходят заголовками, а адрес базы из текста ошибки убираем
    // (`without_url`): он не должен попасть ни клиенту, ни в журнал.
    let resp = match fwd
        .download_request(&kind, &id, state.timeout)
        .send()
        .await
        .map_err(reqwest::Error::without_url)
    {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            warn!(base = %base, kind = %kind, id = %id, error = %e, "таймаут скачивания");
            let secs = state.timeout.as_secs();
            return text_error(
                StatusCode::GATEWAY_TIMEOUT,
                format!("Таймаут скачивания '{id}' из базы '{base}' ({secs} с): {e}"),
            );
        }
        Err(e) => {
            warn!(base = %base, kind = %kind, id = %id, error = %e, "ошибка запроса к базе");
            return text_error(
                StatusCode::BAD_GATEWAY,
                format!("Скачивание не удалось: {e}"),
            );
        }
    };

    let status = resp.status();
    let content_length = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("неизвестен")
        .to_string();

    // HTML вместо файла — запрос не попал в HTTP-сервис 1С (опубликовано без
    // расширения, ошибка публикации, страница логина и т.п.). Показываем начало.
    if is_html(&resp) {
        let head = read_body_head(resp).await;
        warn!(
            base = %base,
            kind = %kind,
            id = %id,
            status = status.as_u16(),
            content_length = %content_length,
            "1С вернула HTML вместо файла"
        );
        return text_error(
            StatusCode::BAD_GATEWAY,
            format!(
                "1С вернула HTML вместо файла (HTTP {status}) — запрос не попал в HTTP-сервис: {head}"
            ),
        );
    }

    info!(
        base = %base,
        kind = %kind,
        id = %id,
        status = status.as_u16(),
        content_length = %content_length,
        "скачивание артефакта"
    );

    let mut builder = Response::builder().status(status);
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_DISPOSITION,
        header::CONTENT_LENGTH,
    ] {
        if let Some(v) = resp.headers().get(&name) {
            builder = builder.header(name, v);
        }
    }

    // Пермит живёт ровно столько, сколько поток тела: он переехал в замыкание
    // `map`, а значит освободится только когда тело допито или брошено.
    let stream = resp.bytes_stream().map(move |chunk| {
        let _keep = &permit;
        chunk
    });

    match builder.body(Body::from_stream(stream)) {
        Ok(r) => r,
        Err(e) => {
            warn!(base = %base, kind = %kind, id = %id, error = %e, "не удалось собрать ответ");
            text_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Не удалось собрать ответ скачивания.".to_string(),
            )
        }
    }
}

/// Ответ 1С объявлен как HTML?
pub(crate) fn is_html(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"))
}

/// Начало тела ответа (до [`HTML_SNIPPET_BYTES`] байт) для сообщения об ошибке.
/// Обрезка — по границе символа, чтобы не порвать UTF-8.
pub(crate) async fn read_body_head(mut resp: reqwest::Response) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < HTML_SNIPPET_BYTES {
        match resp.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut end = text.len().min(HTML_SNIPPET_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Единообразный текстовый ответ на свою ошибку маршрута.
pub(crate) fn text_error(status: StatusCode, msg: String) -> Response {
    let mut resp = Response::new(Body::from(msg));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::extract::{Path, Query, State};
    use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
    use axum::response::Response;
    use axum::routing::get;
    use axum::Router;
    use rmcp::model::CallToolResult;
    use serde_json::json;

    use super::{
        download, encode_component, forward_url, patch_export_result, public_download_url,
        rewrite_hint, routes, DownloadQuery, DownloadState,
    };
    use crate::config::BaseEntry;
    use crate::forward::{service_base, BaseForwarder};

    /// Подсказка `export_artifact` до переписывания.
    const ПОДСКАЗКА: &str = "Артефакт выгружен.\n\
Файл: /tmp/report.epf\n\
Размер: 1024 байт\n\
SHA-256: 3f2a1b\n\
Скачивание: GET http://old-host/download/base-a?kind=additional&id=report.epf";

    /// Тестовый пароль базы — на нём проверяется заголовок Basic Auth.
    const ТЕСТОВЫЙ_ПАРОЛЬ: &str = "pass";

    /// Описание базы для тестов. Адрес недостижимый: в юнит-тестах до сети не доходит.
    fn база(name: &str, url: &str, max_concurrent: Option<usize>) -> BaseEntry {
        serde_json::from_value(json!({
            "name": name,
            "url": url,
            "username": "user",
            "password": ТЕСТОВЫЙ_ПАРОЛЬ,
            "service_root": "mcp",
            "max_concurrent": max_concurrent,
        }))
        .expect("валидное описание базы")
    }

    fn состояние(entries: &[BaseEntry], timeout_secs: u64) -> DownloadState {
        let client = reqwest::Client::new();
        let mut bases = HashMap::new();
        let mut order = Vec::new();
        for e in entries {
            bases.insert(e.name.clone(), BaseForwarder::new(e, client.clone()));
            order.push(e.name.clone());
        }
        DownloadState::new(
            Arc::new(bases),
            Arc::new(order),
            Duration::from_secs(timeout_secs),
        )
    }

    fn параметры(kind: Option<&str>, id: Option<&str>) -> Query<DownloadQuery> {
        Query(DownloadQuery {
            kind: kind.map(str::to_string),
            id: id.map(str::to_string),
        })
    }

    /// Строковый заголовок ответа (или пустая строка, если его нет).
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
            "content": [{ "type": "text", "text": text }],
            "isError": is_error
        }))
        .expect("валидная форма CallToolResult")
    }

    fn текст_результата(res: &CallToolResult) -> String {
        let value = serde_json::to_value(res).expect("сериализация CallToolResult");
        value["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn плохие_параметры_отбиваются_до_сети() {
        let st = состояние(&[база("b", "http://127.0.0.1:1/base", None)], 5);

        let без_kind = download(
            State(st.clone()),
            Path("b".to_string()),
            параметры(None, Some("report.epf")),
        )
        .await;
        assert_eq!(без_kind.status(), StatusCode::BAD_REQUEST);

        let чужой_kind = download(
            State(st.clone()),
            Path("b".to_string()),
            параметры(Some("other"), Some("report.epf")),
        )
        .await;
        assert_eq!(чужой_kind.status(), StatusCode::BAD_REQUEST);

        let без_id = download(
            State(st.clone()),
            Path("b".to_string()),
            параметры(Some("additional"), None),
        )
        .await;
        assert_eq!(без_id.status(), StatusCode::BAD_REQUEST);

        let пустой_id = download(
            State(st),
            Path("b".to_string()),
            параметры(Some("additional"), Some("   ")),
        )
        .await;
        assert_eq!(пустой_id.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn неизвестная_база_даёт_404_с_подсказкой() {
        let st = состояние(&[база("b", "http://127.0.0.1:1/base", None)], 5);
        let resp = download(
            State(st),
            Path("нет-такой".to_string()),
            параметры(Some("additional"), Some("report.epf")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            заголовок(resp.headers(), header::CONTENT_TYPE.as_str()),
            "text/plain; charset=utf-8"
        );
        let text = тело_ответа(resp).await;
        assert!(
            text.contains("Неизвестная база 'нет-такой'"),
            "текст ответа: {text}"
        );
        assert!(text.contains("b"), "перечислены допустимые базы: {text}");
    }

    #[test]
    fn адрес_пересылки_идёт_в_корень_сервиса() {
        let root = service_base("http://h/b/", "/mcp/");
        assert_eq!(root, "http://h/b/hs/mcp");

        let url = forward_url(&root, "additional", "Отчёт (1).epf");
        assert_eq!(
            url,
            "http://h/b/hs/mcp/?download=1&kind=additional&id=%D0%9E%D1%82%D1%87%D1%91%D1%82%20%281%29.epf"
        );
        assert!(!url.contains('+'), "пробел кодируется как %20, а не +");
        assert!(url.contains("%281%29"), "скобки кодируются: {url}");
    }

    #[test]
    fn внешний_адрес_кодирует_все_три_части() {
        assert_eq!(
            public_download_url("http://router:8014", "base-a", "extension", "Отчёт (1).epf"),
            "http://router:8014/download/base-a?kind=extension&id=%D0%9E%D1%82%D1%87%D1%91%D1%82%20%281%29.epf"
        );
        assert_eq!(
            encode_component("база б"),
            "%D0%B1%D0%B0%D0%B7%D0%B0%20%D0%B1"
        );
        assert_eq!(encode_component("a-b.c_d~e"), "a-b.c_d~e");
    }

    #[test]
    fn подсказка_с_блоком_переписывается() {
        let url = "http://router:8014/download/base-a?kind=additional&id=report.epf";
        let text = rewrite_hint(ПОДСКАЗКА, url).expect("SHA-256 в тексте — подсказка меняется");

        assert!(text.contains("Файл: /tmp/report.epf"), "текст: {text}");
        assert!(text.contains("Размер: 1024 байт"), "текст: {text}");
        assert!(text.contains("SHA-256: 3f2a1b"), "текст: {text}");
        assert!(!text.contains("old-host"), "старый адрес убран: {text}");
        assert!(
            text.contains(&format!("Скачивание: GET {url}")),
            "текст: {text}"
        );
        assert_eq!(
            text.matches("Скачивание:").count(),
            1,
            "блок ровно один: {text}"
        );
    }

    #[test]
    fn подсказка_без_блока_дописывается() {
        let без_блока = "Артефакт выгружен.\nФайл: /tmp/report.epf\nSHA-256: 3f2a1b";
        let url = "http://router:8014/download/base-a?kind=additional&id=report.epf";
        let text = rewrite_hint(без_блока, url).expect("SHA-256 в тексте");

        assert!(
            text.starts_with(без_блока),
            "начало текста не тронуто: {text}"
        );
        assert!(
            text.contains(&format!("\n\nСкачивание: GET {url}")),
            "текст: {text}"
        );
    }

    #[test]
    fn без_sha256_подсказка_не_меняется() {
        let url = "http://router:8014/download/base-a?kind=additional&id=report.epf";
        assert!(rewrite_hint("Артефакт не найден: нет такого файла", url).is_none());
    }

    #[test]
    fn подсказка_в_результате_патчится() {
        let url = "http://router:8014/download/base-a?kind=additional&id=report.epf";
        let после = patch_export_result(результат(ПОДСКАЗКА, false), url);
        let text = текст_результата(&после);

        assert!(
            text.contains(&format!("Скачивание: GET {url}")),
            "текст: {text}"
        );
        assert!(!text.contains("old-host"), "старый адрес убран: {text}");
    }

    #[test]
    fn результат_без_sha256_не_меняется() {
        let без = "Артефакт не найден: нет такого файла";
        let после = patch_export_result(результат(без, false), "http://router:8014");
        assert_eq!(текст_результата(&после), без);
    }

    #[test]
    fn ошибка_инструмента_не_меняется() {
        let url = "http://router:8014/download/base-a?kind=additional&id=report.epf";
        let после = patch_export_result(результат(ПОДСКАЗКА, true), url);
        assert_eq!(текст_результата(&после), ПОДСКАЗКА);
    }

    // --- Сквозной тест: поддельная 1С (axum) и роутер (axum) на случайных портах ---

    /// Заголовки одного запроса, дошедшего до поддельной 1С.
    #[derive(Clone, Debug)]
    struct Заголовки1С {
        authorization: String,
        /// Был ли заголовок `X-MCP-Key` (пустое значение — тоже присутствие).
        has_mcp_key: bool,
    }

    #[derive(Clone)]
    struct Фейковая1С {
        файл: Arc<Vec<u8>>,
        имя_файла: Arc<String>,
        ожидаемая_авторизация: Arc<String>,
        видели: Arc<Mutex<Vec<Заголовки1С>>>,
    }

    /// Ответ с текстовым телом и заданным Content-Type.
    fn ответ(status: StatusCode, тело: &str, content_type: &'static str) -> Response {
        let mut resp = Response::new(axum::body::Body::from(тело.to_string()));
        *resp.status_mut() = status;
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        resp
    }

    async fn фейковая_1с(
        State(st): State<Фейковая1С>,
        Query(q): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> Response {
        let авторизация = заголовок(&headers, header::AUTHORIZATION.as_str());
        // Присутствие заголовка, а не его значение: пустой X-MCP-Key — тоже ключ.
        let has_mcp_key = headers.contains_key("X-MCP-Key");
        st.видели
            .lock()
            .expect("журнал заголовков")
            .push(Заголовки1С {
                authorization: авторизация.clone(),
                has_mcp_key,
            });

        if авторизация != *st.ожидаемая_авторизация || has_mcp_key {
            return ответ(
                StatusCode::UNAUTHORIZED,
                "нет доступа",
                "text/plain; charset=utf-8",
            );
        }

        match q.get("id").map(String::as_str) {
            Some("bin") => {
                let mut resp = Response::new(axum::body::Body::from(st.файл.as_ref().clone()));
                resp.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
                let имя = format!(
                    "attachment; filename*=UTF-8''{}",
                    encode_component(&st.имя_файла)
                );
                resp.headers_mut().insert(
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_str(&имя).expect("валидный Content-Disposition"),
                );
                resp.headers_mut().insert(
                    header::CONTENT_LENGTH,
                    HeaderValue::from_str(&st.файл.len().to_string())
                        .expect("валидный Content-Length"),
                );
                resp
            }
            Some("missing") => ответ(
                StatusCode::NOT_FOUND,
                r#"{"error":"Артефакт не найден"}"#,
                "application/json",
            ),
            Some("html") => ответ(
                StatusCode::OK,
                &format!("<html><body>{}</body></html>", "шум ".repeat(200)),
                "text/html; charset=utf-8",
            ),
            _ => ответ(
                StatusCode::NOT_FOUND,
                "неизвестный id",
                "text/plain; charset=utf-8",
            ),
        }
    }

    /// Поднять поддельную 1С и вернуть URL её базы (`http://host:port/base`).
    async fn поднять_фейк(st: Фейковая1С) -> String {
        let app = Router::new()
            .route("/base/hs/mcp/", get(фейковая_1с))
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

    /// Поднять роутер скачивания и вернуть его адрес.
    async fn поднять_роутер(st: DownloadState) -> String {
        let app = routes(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind роутера");
        let addr = listener.local_addr().expect("адрес роутера");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn сквозное_скачивание_через_роутер() {
        // Псевдослучайные байты: файл заведомо не сжимается до константы.
        let файл: Vec<u8> = (0..1024 * 1024)
            .map(|i| ((i * 31 + 7) % 251) as u8)
            .collect();
        assert!(файл.iter().any(|b| *b != файл[0]), "байты не выродились");
        let файл = Arc::new(файл);
        let имя_файла = Arc::new("дополнительный отчёт (1).zip".to_string());
        let видели: Arc<Mutex<Vec<Заголовки1С>>> = Arc::new(Mutex::new(Vec::new()));

        let фейк = поднять_фейк(Фейковая1С {
            файл: файл.clone(),
            имя_файла: имя_файла.clone(),
            // base64("user:pass") — то, что шлёт reqwest.basic_auth("user", "pass").
            ожидаемая_авторизация: Arc::new("Basic dXNlcjpwYXNz".to_string()),
            видели: видели.clone(),
        })
        .await;

        let st = состояние(&[база("mybase", &фейк, Some(1))], 30);
        let роутер = поднять_роутер(st).await;
        let client = reqwest::Client::new();

        // 1. Файл доходит байт в байт вместе с заголовками 1С.
        let r = client
            .get(format!("{роутер}/download/mybase?kind=additional&id=bin"))
            .send()
            .await
            .expect("запрос на скачивание");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            заголовок(r.headers(), header::CONTENT_DISPOSITION.as_str()),
            format!(
                "attachment; filename*=UTF-8''{}",
                encode_component(&имя_файла)
            )
        );
        assert_eq!(
            заголовок(r.headers(), header::CONTENT_LENGTH.as_str()),
            файл.len().to_string()
        );
        let тело = r.bytes().await.expect("тело файла");
        let ожидаемое: &[u8] = файл.as_slice();
        assert_eq!(тело.len(), ожидаемое.len(), "длина тела");
        assert_eq!(тело.as_ref(), ожидаемое, "тело совпадает байт в байт");

        // 2. «Не найден»: статус и JSON-тело 1С отдаются клиенту как есть.
        let r = client
            .get(format!("{роутер}/download/mybase?kind=supplied&id=missing"))
            .send()
            .await
            .expect("запрос на отсутствующий артефакт");
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let json: serde_json::Value = r.json().await.expect("JSON от 1С");
        assert_eq!(json["error"], "Артефакт не найден");

        // 3. HTML вместо файла — 502 с исходным статусом 1С в тексте.
        let r = client
            .get(format!("{роутер}/download/mybase?kind=extension&id=html"))
            .send()
            .await
            .expect("запрос с HTML в ответе");
        assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
        let text = r.text().await.expect("тело ошибки");
        assert!(text.contains("HTTP 200"), "статус 1С в тексте: {text}");
        assert!(text.contains("<html>"), "начало HTML в тексте: {text}");

        // 4. Поддельная 1С увидела ровно ожидаемые заголовки (401 ни разу не отдала).
        {
            let видели = видели.lock().expect("журнал заголовков");
            assert_eq!(видели.len(), 3, "до 1С дошло три запроса");
            for заголовки in видели.iter() {
                assert_eq!(заголовки.authorization, "Basic dXNlcjpwYXNz");
                assert!(
                    !заголовки.has_mcp_key,
                    "роутер не отправляет X-MCP-Key в 1С"
                );
            }
        }

        // 4б. Пустое значение X-MCP-Key — тоже присутствие заголовка: прямой
        // запрос к поддельной 1С с пустым ключом получает отказ.
        let r = client
            .get(format!("{фейк}/hs/mcp/?download=1&kind=additional&id=bin"))
            .basic_auth("user", Some("pass"))
            .header("X-MCP-Key", "")
            .send()
            .await
            .expect("прямой запрос к поддельной 1С");
        assert_eq!(
            r.status(),
            StatusCode::UNAUTHORIZED,
            "пустой X-MCP-Key тоже считается присутствием заголовка"
        );

        // 5. max_concurrent=1: пермит освобождается после передачи, два скачивания
        // подряд проходят (иначе второе зависло бы в очереди семафора).
        let url = format!("{роутер}/download/mybase?kind=additional&id=bin");
        for _ in 0..2 {
            let r = tokio::time::timeout(Duration::from_secs(10), client.get(url.clone()).send())
                .await
                .expect("скачивание зависло: пермит семафора не освободился")
                .expect("запрос на скачивание");
            assert_eq!(r.status(), StatusCode::OK);
            let тело = r.bytes().await.expect("тело файла");
            assert_eq!(тело.len(), файл.len());
        }
    }
}
