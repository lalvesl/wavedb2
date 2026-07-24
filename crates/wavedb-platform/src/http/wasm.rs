//! Browser `post`: `fetch` + `Request`, the response body streamed through
//! a `ReadableStreamDefaultReader`.

use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{ReadableStreamDefaultReader, Request, RequestInit, Response};

use crate::error::{Error, Result, js};

/// POST `body` to `addr` and hand back the response body, requiring a
/// `200`. A bare `host:port` gets `http://` prepended; a full URL passes
/// through (a page served over https must name an https node).
///
/// # Errors
/// [`Error::Status`] on a non-200 answer, [`Error::Js`] on a browser API
/// refusal (including running outside a `window` context).
pub async fn post(addr: &str, body: &[u8]) -> Result<Body> {
    let url = if addr.contains("://") {
        addr.to_owned()
    } else {
        format!("http://{addr}/")
    };
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_body(&Uint8Array::from(body).into());
    let request = Request::new_with_str_and_init(&url, &init)
        .map_err(|e| js("Request::new", &e))?;
    request
        .headers()
        .set("content-type", "application/octet-stream")
        .map_err(|e| js("headers.set", &e))?;
    let window = web_sys::window()
        .ok_or_else(|| Error::Js(String::from("no window")))?;
    let response: Response =
        JsFuture::from(window.fetch_with_request(&request))
            .await
            .map_err(|e| js("fetch", &e))?
            .dyn_into()
            .map_err(|e| js("fetch did not yield a Response", &e))?;
    let status = response.status();
    if status != 200 {
        return Err(Error::Status(status));
    }
    // A bodyless response (fetch may elide one) reads as an empty stream.
    let reader = response
        .body()
        .map(|stream| {
            stream
                .get_reader()
                .dyn_into::<ReadableStreamDefaultReader>()
        })
        .transpose()
        .map_err(|e| js("body.getReader", &e.into()))?;
    Ok(Body { reader })
}

/// A response body streaming in from the fetch's `ReadableStream`.
#[derive(Debug)]
pub struct Body {
    /// `None` once the stream reported done (or was never there).
    reader: Option<ReadableStreamDefaultReader>,
}

impl Body {
    /// The next run of body bytes in arrival order; `None` = the stream is
    /// done (mid-frame is the caller's judgement).
    ///
    /// # Errors
    /// [`Error::Js`] when the underlying stream read rejects.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(reader) = &self.reader else {
            return Ok(None);
        };
        let step = JsFuture::from(reader.read())
            .await
            .map_err(|e| js("reader.read", &e))?;
        let done = Reflect::get(&step, &JsValue::from_str("done"))
            .map_err(|e| js("read.done", &e))?
            .as_bool()
            .unwrap_or(true);
        if done {
            self.reader = None;
            return Ok(None);
        }
        let value = Reflect::get(&step, &JsValue::from_str("value"))
            .map_err(|e| js("read.value", &e))?;
        Ok(Some(Uint8Array::new(&value).to_vec()))
    }
}
