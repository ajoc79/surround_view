use crate::{
    media::{Event, Frame, VideoInfo, valid_dimensions},
    ts::Demux,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(module = "/web/decoder.js")]
extern "C" {
    #[wasm_bindgen(js_name = BrowserDecoder)]
    type BrowserDecoder;

    #[wasm_bindgen(constructor)]
    fn new(demux: Demux) -> BrowserDecoder;

    #[wasm_bindgen(method, js_name = chooseFile)]
    fn choose_file(this: &BrowserDecoder);
    #[wasm_bindgen(method, js_name = takeOpenedFile)]
    fn take_opened_file(this: &BrowserDecoder) -> bool;
    #[wasm_bindgen(method)]
    fn seek(this: &BrowserDecoder, position: f64);
    #[wasm_bindgen(method)]
    fn poll(this: &BrowserDecoder) -> JsValue;
    #[wasm_bindgen(method)]
    fn close(this: &BrowserDecoder);
}

pub struct Video(BrowserDecoder);

impl Video {
    pub fn new() -> Self {
        let decoder = BrowserDecoder::new(Demux::new());
        decoder.choose_file();
        Self(decoder)
    }

    pub fn choose_file(&mut self) {
        self.0.choose_file();
    }

    pub fn take_opened_file(&mut self) -> bool {
        self.0.take_opened_file()
    }

    pub fn seek(&mut self, position: f64) {
        self.0.seek(position);
    }

    pub fn poll(&mut self) -> Option<Event> {
        let event = self.0.poll();
        if event.is_null() || event.is_undefined() {
            return None;
        }
        let get = |key: &str| {
            js_sys::Reflect::get(&event, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
        };
        let text = |name| get(name).as_string().unwrap_or_default();
        let number = |name| get(name).as_f64().unwrap_or_default();
        let width = number("width") as u32;
        let height = number("height") as u32;
        match text("kind").as_str() {
            "opened" => {
                if !valid_dimensions(width, height) {
                    return Some(Event::Failed(format!(
                        "Unsupported camera mosaic: {width} × {height}"
                    )));
                }
                Some(Event::Opened(VideoInfo {
                    name: text("name"),
                    duration: number("duration"),
                    width,
                    height,
                }))
            }
            "frame" if valid_dimensions(width, height) => {
                let bytes = js_sys::Uint8Array::from(get("data")).to_vec();
                if bytes.len() != (width * height * 4) as usize {
                    return Some(Event::Failed("Unexpected WebCodecs frame layout".into()));
                }
                Some(Event::Frame(Frame {
                    pts: number("pts"),
                    width,
                    height,
                    rgba: bytes,
                }))
            }
            "frame" => Some(Event::Failed(
                "H.264 frame resolution is unsupported".into(),
            )),
            "warning" => Some(Event::Warning(text("message"))),
            "error" => Some(Event::Failed(text("message"))),
            "end" => Some(Event::End),
            _ => None,
        }
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.0.close();
    }
}
