//! The browser shell: web `Services`, drag-and-drop, and the eframe web runner.

use deckcraft_engine::Session;
use deckcraft_ui_egui::{Inbox, InboxEvent, InboxFile, Services, SlideApp};
use wasm_bindgen::JsCast as _;

const PRESENTATION_EXTS: &[&str] = &["deckcraft", "pptx", "potx", "ppsx", "txt", "md"];
const PICTURE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];
const AUDIO_EXTS: &[&str] = &["wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "aif", "aiff"];
const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mov", "webm"];
const CANVAS_ID: &str = "deckcraft_canvas";
const LOADING_ID: &str = "deckcraft_loading";

/// Start the browser shell: install logging, locate the canvas, launch the eframe runner,
/// build the sample or blank deck, and replace the loading element when launch finishes.
pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    wasm_bindgen_futures::spawn_local(async {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            log::error!("no document");
            return;
        };
        let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            log::error!("missing <canvas id=\"{CANVAS_ID}\">");
            return;
        };
        let mut options = eframe::WebOptions::default();
        if query().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(move |cc| {
                    if let Some(rs) = &cc.wgpu_render_state {
                        log::info!("deckcraft-web {}: wgpu backend {:?}", env!("CARGO_PKG_VERSION"), rs.adapter.get_info().backend);
                    }
                    deckcraft_ui_egui::install_ime_pass_hooks(&cc.egui_ctx);
                    if let Some(token) = crate::presenter::token() {
                        let presenter = crate::presenter::Popup::new(token, cc.egui_ctx.clone()).map_err(std::io::Error::other)?;
                        return Ok(Box::new(presenter) as Box<dyn eframe::App>);
                    }
                    let inbox: Inbox = Inbox::default();
                    let mut app = SlideApp::new(Session::new(), services(inbox.clone(), cc.egui_ctx.clone()));
                    let q = query();
                    let cloud = q.contains("cloud");
                    app.cloud_file_mode = cloud;
                    if !cloud {
                        if !q.contains("blank") {
                            if let Err(e) = deckcraft_engine::sample::open_sample(&mut app.session) {
                                log::error!("sample deck: {e}");
                            }
                        } else if let Err(e) = app.run("file.new", serde_json::json!({})) {
                            log::error!("new presentation: {e}");
                        }
                    }
                    if q.contains("show") {
                        app.start_show(0, false);
                    }
                    Ok(Box::new(WebShell { app, inbox }))
                }),
            )
            .await;
        if let Some(el) = document.get_element_by_id(LOADING_ID) {
            match result {
                Ok(()) => el.remove(),
                Err(e) => el.set_inner_html(&format!("<p>DeckCraft failed to start: {e:?}</p><p>A browser with WebGPU or WebGL2 is required.</p>")),
            }
        }
    });
}

fn query() -> String {
    web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
}

/// Wraps the app to read dropped files asynchronously (browsers can't read them synchronously)
/// and feed them through the inbox.
struct WebShell {
    app: SlideApp,
    inbox: Inbox,
}

impl eframe::App for WebShell {
    /// Delegate one eframe logic pass to the wrapped application.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic(ctx);
        if self.app.show.is_none()
            && let Some(host) = self.app.services.presenter.as_mut()
        {
            host.close();
        }
    }

    /// Consume browser drops with fresh input, capture the identity before the asynchronous read,
    /// and deliver the id, name, and bytes through the inbox.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Browsers cannot read dropped files synchronously. Consume this pass's drops here, with
        // fresh input, and capture the document/slide before the asynchronous read finishes.
        for f in std::mem::take(&mut raw.dropped_files) {
            let id = self.app.begin_external_file("drop");
            let inbox = self.inbox.clone();
            let ctx = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let name = f.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "dropped".into());
                match f.bytes_async().await {
                    Ok(bytes) => {
                        push_inbox(&inbox, InboxEvent::File(InboxFile { id, name, bytes }));
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        log::error!("couldn't read dropped file {name}: {e}");
                        push_inbox(&inbox, InboxEvent::Cancelled { id });
                        ctx.request_repaint();
                    }
                }
            });
        }
        self.app.raw_input_hook(ctx, raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

/// Web services: no file paths, so `pick_open`/`pick_save`/`read` stay `None` and the UI falls
/// back to `open_async` (browser picker → inbox) and `download`.
fn services(inbox: Inbox, ctx: egui::Context) -> Services {
    let open_inbox = inbox.clone();
    let presenter = match crate::presenter::BrowserHost::new(ctx.clone()) {
        Ok(host) => Some(Box::new(host) as Box<dyn deckcraft_ui_egui::presenter::Host>),
        Err(error) => {
            log::error!("Presenter window: {error}");
            None
        }
    };
    Services {
        presenter,
        open_async: Some(Box::new(move |purpose: &str, request: u64| {
            let inbox = open_inbox.clone();
            let ctx = ctx.clone();
            let dialog = match purpose {
                "picture" => rfd::AsyncFileDialog::new().add_filter("Pictures", PICTURE_EXTS),
                "audio" => rfd::AsyncFileDialog::new().add_filter("Audio", AUDIO_EXTS),
                "video" => rfd::AsyncFileDialog::new().add_filter("Video", VIDEO_EXTS),
                _ => rfd::AsyncFileDialog::new().add_filter("Presentations", PRESENTATION_EXTS),
            };
            wasm_bindgen_futures::spawn_local(async move {
                let Some(file) = dialog.pick_file().await else {
                    push_inbox(&inbox, InboxEvent::Cancelled { id: request });
                    ctx.request_repaint();
                    return;
                };
                let bytes = file.read().await;
                push_inbox(&inbox, InboxEvent::File(InboxFile { id: request, name: file.file_name(), bytes }));
                ctx.request_repaint();
            });
        })),
        write: Some(Box::new(|name: &str, bytes: &[u8]| download(name, bytes))),
        download: Some(Box::new(|name: &str, bytes: &[u8]| {
            if let Err(e) = download(name, bytes) {
                log::error!("download of {name} failed: {e}");
            }
        })),
        inbox: Some(inbox),
        ..Default::default()
    }
}

fn push_inbox(inbox: &Inbox, event: InboxEvent) {
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push(event);
}

/// Trigger a browser save dialog or download of `bytes` named after the last component of `path`.
fn download(path: &str, bytes: &[u8]) -> Result<(), String> {
    use wasm_bindgen::JsValue;
    let js = |e: JsValue| format!("{e:?}");
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "deckcraft.pptx".into());
    let window = web_sys::window().ok_or("no window")?;

    let show_picker_fn: Option<js_sys::Function> =
        js_sys::Reflect::get(&window, &JsValue::from_str("showSaveFilePicker")).ok().and_then(|v| v.dyn_into().ok());

    if let Some(picker_fn) = show_picker_fn {
        let ext = std::path::Path::new(&name).extension().and_then(std::ffi::OsStr::to_str).unwrap_or("pptx").to_ascii_lowercase();
        let dot_ext = format!(".{ext}");
        let mime = mime_for(&name);

        let accept_obj = js_sys::Object::new();
        let exts_arr = js_sys::Array::new();
        exts_arr.push(&JsValue::from_str(&dot_ext));
        let _ = js_sys::Reflect::set(&accept_obj, &JsValue::from_str(mime), &exts_arr);

        let type_desc = match ext.as_str() {
            "pptx" => "PowerPoint Presentation (*.pptx)",
            "potx" => "PowerPoint Template (*.potx)",
            "ppsx" => "PowerPoint Show (*.ppsx)",
            "deckcraft" => "DeckCraft Presentation (*.deckcraft)",
            "pdf" => "PDF Document (*.pdf)",
            "png" => "PNG Image (*.png)",
            "jpg" | "jpeg" => "JPEG Image (*.jpeg)",
            "gif" => "Animated GIF (*.gif)",
            "rtf" => "Rich Text Format (*.rtf)",
            "txt" => "Plain Text (*.txt)",
            _ => "Presentation",
        };

        let type_obj = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&type_obj, &JsValue::from_str("description"), &JsValue::from_str(type_desc));
        let _ = js_sys::Reflect::set(&type_obj, &JsValue::from_str("accept"), &accept_obj);

        let types_arr = js_sys::Array::new();
        types_arr.push(&type_obj);

        let picker_opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&picker_opts, &JsValue::from_str("suggestedName"), &JsValue::from_str(&name));
        let _ = js_sys::Reflect::set(&picker_opts, &JsValue::from_str("types"), &types_arr);

        let prom_val = picker_fn.call1(&window, &picker_opts).map_err(js)?;
        let prom: js_sys::Promise = prom_val.dyn_into().map_err(|_| "not a promise".to_string())?;

        let bytes_vec = bytes.to_vec();
        let name_owned = name.clone();
        let fallback = move |bytes: &[u8]| {
            if let Some(w) = web_sys::window()
                && let Err(e) = trigger_anchor_download(&w, &name_owned, bytes)
            {
                log::error!("download fallback failed: {e}");
            }
        };
        wasm_bindgen_futures::spawn_local(async move {
            match wasm_bindgen_futures::JsFuture::from(prom).await {
                Ok(handle_val) => {
                    if let Err(e) = write_file_handle(&handle_val, &bytes_vec).await {
                        log::error!("failed writing to file handle: {e}");
                        fallback(&bytes_vec);
                    } else {
                        log::info!("Successfully saved file through save file picker");
                    }
                }
                Err(e) => {
                    let err_name = js_sys::Reflect::get(&e, &JsValue::from_str("name")).ok().and_then(|v| v.as_string()).unwrap_or_default();
                    if err_name == "AbortError" {
                        log::info!("Save file dialog cancelled by user");
                    } else {
                        log::warn!("showSaveFilePicker refused ({err_name}); downloading instead");
                        fallback(&bytes_vec);
                    }
                }
            }
        });
        return Ok(());
    }

    trigger_anchor_download(&window, &name, bytes)
}

async fn write_file_handle(handle: &wasm_bindgen::JsValue, bytes: &[u8]) -> Result<(), String> {
    use wasm_bindgen::JsValue;
    let js = |e: JsValue| format!("{e:?}");
    let create_fn: js_sys::Function = js_sys::Reflect::get(handle, &JsValue::from_str("createWritable"))
        .map_err(js)?
        .dyn_into()
        .map_err(|_| "createWritable is not a function".to_string())?;
    let stream_prom: js_sys::Promise = create_fn.call0(handle).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    let stream = wasm_bindgen_futures::JsFuture::from(stream_prom).await.map_err(js)?;

    let write_fn: js_sys::Function =
        js_sys::Reflect::get(&stream, &JsValue::from_str("write")).map_err(js)?.dyn_into().map_err(|_| "write is not a function".to_string())?;
    let uint8 = js_sys::Uint8Array::from(bytes);
    let write_prom: js_sys::Promise = write_fn.call1(&stream, &uint8).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    wasm_bindgen_futures::JsFuture::from(write_prom).await.map_err(js)?;

    let close_fn: js_sys::Function =
        js_sys::Reflect::get(&stream, &JsValue::from_str("close")).map_err(js)?.dyn_into().map_err(|_| "close is not a function".to_string())?;
    let close_prom: js_sys::Promise = close_fn.call0(&stream).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    wasm_bindgen_futures::JsFuture::from(close_prom).await.map_err(js)?;
    Ok(())
}

fn trigger_anchor_download(window: &web_sys::Window, name: &str, bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let document = window.document().ok_or("no document")?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime_for(name));
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
    let a: web_sys::HtmlAnchorElement = document.create_element("a").map_err(js)?.dyn_into().map_err(|_| "not an anchor")?;
    a.set_href(&url);
    a.set_download(name);
    a.style().set_property("display", "none").map_err(js)?;
    let body = document.body().ok_or("no body")?;
    body.append_child(&a).map_err(js)?;
    a.click();
    a.remove();
    // Revoke after the click has been dispatched; the download keeps its own reference.
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        web_sys::Url::revoke_object_url(&url).ok();
    });
    window.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 10_000).map_err(js)?;
    Ok(())
}

fn mime_for(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("pdf") => "application/pdf",
        Some("gif") => "image/gif",
        Some("txt") => "text/plain",
        Some("rtf") => "application/rtf",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("potx") => "application/vnd.openxmlformats-officedocument.presentationml.template",
        Some("ppsx") => "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
        Some("deckcraft") => "application/zip",
        _ => "application/octet-stream",
    }
}
